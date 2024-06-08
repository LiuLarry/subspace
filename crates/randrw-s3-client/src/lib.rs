#![feature(lazy_cell)]

use std::time::Instant;
use std::{cmp::min, io, vec};
use std::sync::{LazyLock, Arc};
use std::collections::HashMap;
use std::io::Read;

use log::info;
use dirs;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio_util::bytes::{Buf, Bytes};
use tokio_util::io::StreamReader;

use anyhow::{anyhow, Result};
use rust_lapper::{Interval, Lapper};

use aws_types::region::Region;
use aws_types::SdkConfig;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Credentials, SharedCredentialsProvider};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};

use futures_util::{Stream, TryFutureExt};
use futures_util::TryStreamExt;

use crate::datastore::Datastore;

mod datastore;

static SERVER_URL: LazyLock<String> = LazyLock::new(|| {
    std::env::var("RANDRW_S3_SERVER").unwrap()
});

static S3_CLIENT_CONTEXT: LazyLock<Arc<Context>> = LazyLock::new(|| {
    get_s3_client().unwrap()
});

pub async fn object_exist(
    key: &str,
) -> bool {
    S3_CLIENT_CONTEXT.datastore.read().unwrap().get(&key).is_some()
}

// 447 MB
static DEFAULT_UPLOAD_PART_SIZE: usize = 447 * 1024 * 1024;

async fn multipart_upload(
    client: &Client,
    bucket: &str,
    key: &str,
    mut body: Bytes,
    part_size: usize,
) -> Result<()> {
    let upload_out = client.create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await?;

    let upload_id = upload_out
        .upload_id()
        .ok_or_else(|| anyhow!("{}, must need upload id", key))?;

    let mut etags = Vec::new();
    let mut part_num = 1;

    // TODO 并发优化
    while body.len() > 0 {
        let read_size = min(part_size, body.len());
        let upload_data = body.split_to(read_size);

        let etag = client.upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_num)
            .body(ByteStream::from(upload_data))
            .send()
            .await?
            .e_tag
            .ok_or_else(|| anyhow!("{} must need e_tag", key))?;

        etags.push(etag);
        part_num += 1;
    }

    let parts = etags.into_iter()
        .enumerate()
        .map(|(i, e_tag)| {
            CompletedPart::builder()
                .part_number(i as i32 + 1)
                .e_tag(e_tag)
                .build()
        })
        .collect::<Vec<_>>();

    client.complete_multipart_upload()
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .set_parts(Some(parts))
                .build(),
        )
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .send()
        .await?;

    Ok(())
}


pub async fn put_object(
    key: String,
    content_len: u64,
    body: impl Stream<Item=Result<impl Buf, warp::Error>> + Unpin,
) -> Result<()> {

    let s3config = &S3_CLIENT_CONTEXT.s3config;
    let part_size = S3_CLIENT_CONTEXT.part_size;
    let mut body_len = content_len;
    let mut parts_len = content_len / part_size;

    if content_len % part_size != 0 {
        parts_len += 1;
    }

    let lock = S3_CLIENT_CONTEXT.tasks.lock()
        .unwrap()
        .entry(key.clone())
        .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
        .clone();

    let _guard = lock.write().await;

    let mut reader = StreamReader::new(body.map_err(|e| io::Error::new(io::ErrorKind::Other, e)));
    let mut buff = vec![0u8; part_size as usize];
    let mut parts_num = 0;

    let (_notify, notified) = tokio::sync::watch::channel(());
    let sem = tokio::sync::Semaphore::new(4);
    let sem = Arc::new(sem);

    let mut futus = Vec::new();

    while body_len > 0 {
        let sem_guard = sem.clone().acquire_owned().await?;

        let read_len = min(part_size, body_len);
        reader.read_exact(&mut buff[..read_len as usize]).await?;

        let mut notified = notified.clone();

        let content_len = S3_CLIENT_CONTEXT.s3client.head_object()
            .bucket(&s3config.bucket)
            .key(format!("{}/{}", key, parts_num))
            .send()
            .await
            .ok()
            .and_then(|out| out.content_length.map(|v| v as u64));

        if content_len != Some(part_size) {
            let s3client = S3_CLIENT_CONTEXT.s3client.clone();
            let bucket = s3config.bucket.clone();
            let key = format!("{}/{}", key, parts_num);
            let upload_data = Bytes::copy_from_slice(&buff);

            let fut = tokio::spawn(async move {
                let fut = async {
                    let _guard = sem_guard; // 这段代码的作用？
                    multipart_upload(&s3client, &bucket, &key, upload_data, DEFAULT_UPLOAD_PART_SIZE).await
                };

                tokio::select! {
                    res = fut => res,
                    _ = notified.changed() => Err(anyhow!("abort task"))
                }
            })
            .map_err(|e| anyhow!(e))
            .and_then(|r| async move { r });

            futus.push(fut);
        }

        body_len -= read_len;
        parts_num += 1;
    }

    futures_util::future::try_join_all(futus).await?;

    let obj = datastore::Object {
        key,
        total_size: content_len,
        part_size,
        parts_len,
    };

    S3_CLIENT_CONTEXT.datastore.write().unwrap().put(obj)?;

    Ok(())
}

// put_zero_object
pub async fn put_zero_object(
    key: &str,
    data_len: u64
)  -> Result<()> {
    let buff = [0u8; 8192];
    let stream = futures_util::stream::repeat_with(|| Ok(buff.as_slice()));

    put_object(
        key.to_string(),
        data_len,
        stream
    ).await
}

#[derive(Clone, Eq, PartialEq)]
pub struct Part {
    pub offset: u64,
    pub data: Vec<u8>,
}

pub async fn update_object(
    key: &str,
    offset: u64,
    mut content_len: u64,
    body: impl Stream<Item=Result<impl Buf, warp::Error>> + Unpin,
) -> Result<()> {
    let s3config = &S3_CLIENT_CONTEXT.s3config;
    let s3client = &S3_CLIENT_CONTEXT.s3client;
    let key = key.to_string();

    let lock = S3_CLIENT_CONTEXT.tasks.lock()
        .unwrap()
        .entry(key.clone())
        .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
        .clone();

    let _guard = lock.read().await;
    let obj = S3_CLIENT_CONTEXT.datastore.read().unwrap().get(&key).ok_or_else(|| anyhow!("{} not found", key))?;
    let part_size = obj.part_size;

    let mut reader = StreamReader::new(body.map_err(|e| io::Error::new(io::ErrorKind::Other, e)));
    let mut buff = vec![0u8; part_size as usize];

    let mut parts_num = offset / part_size;

    // |..{....|......|
    if offset % part_size != 0 {
        let offset_in_part = (offset % part_size) as usize;
        let range = format!("bytes={}-{}", 0, offset_in_part - 1);

        let out = s3client.get_object()
            .bucket(&s3config.bucket)
            .key(format!("{}/{}", key, parts_num))
            .range(range)
            .send()
            .await?;

        let mut s3reader = out.body.into_async_read();
        s3reader.read_exact(&mut buff[..offset_in_part]).await?;

        reader.read_exact(&mut buff[offset_in_part..min(part_size as usize, offset_in_part + content_len as usize)]).await?;
    } else {
        reader.read_exact(&mut buff[..min(part_size as usize, content_len as usize)]).await?;
    }

    // |..{...}.|......|
    if (offset + content_len) / part_size == offset / part_size {
        let offset_in_part = ((offset + content_len) % part_size) as usize;
        let range = format!("bytes={}-{}", offset_in_part, part_size - 1);

        let out = s3client.get_object()
            .bucket(&s3config.bucket)
            .key(format!("{}/{}", key, parts_num))
            .range(range)
            .send()
            .await?;

        let mut s3reader = out.body.into_async_read();
        s3reader.read_exact(&mut buff[offset_in_part..]).await?;

        content_len = 0;
    } else {
        content_len -= part_size - (offset % part_size);
    }

    multipart_upload(s3client, &s3config.bucket, &format!("{}/{}", key, parts_num), Bytes::copy_from_slice(&buff), DEFAULT_UPLOAD_PART_SIZE).await?;

    parts_num += 1;

    // |..{....|....}..|
    while content_len > 0 {
        let read_len = min(part_size, content_len);
        reader.read_exact(&mut buff[..read_len as usize]).await?;

        if read_len < part_size {
            let range = format!("bytes={}-{}", read_len, part_size - 1);

            let out = s3client.get_object()
                .bucket(&s3config.bucket)
                .key(format!("{}/{}", key, parts_num))
                .range(range)
                .send()
                .await?;

            let mut s3reader = out.body.into_async_read();
            s3reader.read_exact(&mut buff[read_len as usize..]).await?;
        }

        multipart_upload(s3client, &s3config.bucket, &format!("{}/{}", key, parts_num), Bytes::copy_from_slice(&buff), DEFAULT_UPLOAD_PART_SIZE).await?;

        content_len -= read_len;
        parts_num += 1;
    }
    Ok(())
    
}

pub async fn get_object_with_ranges(
    key: &str,
    // (start position, length)
    ranges: &[(u64, u64)],
) -> Result<Vec<Part>> {
    let t = Instant::now();
    let s3config = &S3_CLIENT_CONTEXT.s3config;
    let s3client = &S3_CLIENT_CONTEXT.s3client;

    if ranges.is_empty() {
        return Ok(Vec::new());
    }

    let lock = S3_CLIENT_CONTEXT.tasks.lock()
        .unwrap()
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
        .clone();

    let _guard = lock.read().await;

    let obj = S3_CLIENT_CONTEXT.datastore.read().unwrap().get(&key).ok_or_else(|| anyhow!("{} not found", key))?;
    let part_size = obj.part_size;

    let parts = ranges.iter()
        .map(|(offset, len)| {
            Part {
                offset: *offset,
                data: vec![0u8; (*len) as usize],
            }
        })
        .collect::<Vec<_>>();

    let parts = Box::leak(Box::new(parts));
    let parts_ptr = parts as *mut Vec<Part> as usize;

    let keymap: &mut HashMap<u64, Vec<(usize, &mut [u8])>> = Box::leak(Box::new(HashMap::new()));
    let keymap_ptr = keymap as *mut HashMap<u64, Vec<(usize, &mut [u8])>> as usize;

    for ((offset, _), part) in ranges.iter().zip(parts.iter_mut()) {
        let mut offset = *offset;
        let mut buff = part.data.as_mut_slice();
        let mut parts_num = offset / part_size;

        while !buff.is_empty() {
            let offset_in_part = (offset % part_size) as usize;
            let split_len = min(part_size as usize - offset_in_part, buff.len());

            let (l, r) = buff.split_at_mut(split_len);
            buff = r;

            let list = keymap.entry(parts_num).or_insert(Vec::new());
            list.push((offset_in_part, l));

            offset += split_len as u64;
            parts_num += 1;
        }
    }

    let mut futs = Vec::new();
    let (notify, notified) = tokio::sync::watch::channel(());

    for (parts_num, parts) in keymap.iter_mut() {
        let ranges: Vec<(Option<u64>, Option<u64>)> = parts
            .iter()
            .map(|(start, part)| (Some((*start) as u64), Some((*start + part.len() - 1) as u64)))
            .collect();

        let mut ranges_str = String::from("bytes=");
        ranges_str.push_str(&range_to_string(ranges[0]).unwrap());

        for i in 1..ranges.len() {
            ranges_str.push_str(", ");
            ranges_str.push_str(&range_to_string(ranges[i]).unwrap());
        }

        let send = s3client.get_object()
            .bucket(&s3config.bucket)
            .key(format!("{}/{}", key, parts_num))
            .range(ranges_str)
            .send();

        let fut = async move {
                let output = send.await?;
                let content_type = output.content_type.ok_or_else(|| anyhow!("Content-Type can't be empty"))?;
    
                let merge_parts = if content_type.contains("octet-stream") {
                    let range = output.content_range.ok_or_else(|| {
                        anyhow!("Content-Range can't be empty")
                    })?;
    
                    let (start, end, _): (u64, u64, u64) = parse_range(&range)?;
    
                    let len = end - start + 1;
                    let mut buff = Vec::with_capacity(len as usize);
                    output.body.into_async_read().read_to_end(&mut buff).await?;
    
                    let part = Part {
                        offset: start,
                        data: buff,
                    };
                    vec![part]
                } else {
                    let boundary: String = sscanf::scanf!(
                        content_type,
                        "multipart/byteranges; boundary={}",
                        String
                    ).map_or_else(|_| sscanf::scanf!(
                        content_type,
                        "multipart/byteranges;boundary={}",
                        String
                    ), |v| Ok(v))
                        .map_err(|_| anyhow!("get boundary error"))?;
    
                    let mut buff = Vec::new();
                    output.body.into_async_read().read_to_end(&mut buff).await?;
    
                    let mut multipart = multipart::server::Multipart::with_body(buff.as_slice(), boundary);
                    let mut list = Vec::with_capacity(ranges.len());
    
                    while let Some(mut part_field) = multipart.read_entry()? {
                        let range = part_field.headers.content_range.ok_or_else(|| {
                            anyhow!("content-Range can't be empty")
                        })?;
    
                        let (start, end, _): (u64, u64, u64) = parse_range(&range)?;
                        let len = end - start + 1;
    
                        let mut buff = Vec::with_capacity(len as usize);
                        part_field.data.read_to_end(&mut buff)?;
    
                        let part = Part {
                            offset: start,
                            data: buff,
                        };
                        list.push(part);
                    }
                    list
                };
    
                // overlapping
                let v = merge_parts.into_iter()
                    .map(|part| Interval {
                        start: part.offset,
                        stop: part.offset + part.data.len() as u64,
                        val: part
                    })
                    .collect();
    
                let lapper = Lapper::new(v);
    
                for (start, to) in parts {
                    let start = *start;
                    // include
                    let end = start + to.len() - 1;
                    let len = to.len();
    
                    let mut iv_opt = None;
                    let mut it = lapper.find(start as u64, start as u64 + 1);
    
                    while let Some(iv) = it.next() {
                        if start as u64 >= iv.start && (end as u64) < iv.stop {
                            iv_opt = Some(iv);
                            break
                        }
                    };
    
                    let iv = match iv_opt {
                        None => return Err(anyhow!("parse ranges failed")),
                        Some(v) => v,
                    };
    
                    to.copy_from_slice(&iv.val.data[start - iv.start as usize..start - iv.start as usize + len]);
                }
                Ok(())
            };

        let mut notified = notified.clone();

        let fut = tokio::spawn(async move {
            tokio::select! {
                res = fut => res,
                _ = notified.changed() => Err(anyhow!("abort task"))
            }
        })
        .map_err(|e| anyhow!(e))
        .and_then(|r| async move { r });

        futs.push(fut);
    }

    let futs_res = futures_util::future::try_join_all(futs).await;
    drop(notified);
    let _ = notify.send(());
    notify.closed().await;

    let parts;

    unsafe {
        let _ = Box::from_raw(keymap_ptr as *mut HashMap<u64, Vec<(usize, &mut [u8])>>);
        parts = *Box::from_raw(parts_ptr as *mut Vec<Part>);
    }

    futs_res?;
    info!("get object with ranges use: {:?}, ranges: {}", t.elapsed(), ranges.len());
    Ok(parts)
}

#[derive(Debug, Clone, Deserialize)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub part_size: u64
}

struct Context {
    part_size: u64,
    datastore: std::sync::RwLock<Datastore>,
    tasks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::RwLock<()>>>>,
    s3client: Client,
    s3config: S3Config,
}

fn range_to_string(range: (Option<u64>, Option<u64>)) -> Result<String> {
    let str = match range {
        (Some(start), Some(end)) => format!("{}-{}", start, end),
        (Some(start), None) => format!("{}-", start),
        (None, Some(end)) => format!("-{}", end),
        _ => return Err(anyhow!("range params is invalid"))
    };
    Ok(str)
}

// (start, end, total)
fn parse_range(range_str: &str) -> Result<(u64, u64, u64)> {
    sscanf::scanf!(range_str, "bytes {}-{}/{}", u64, u64, u64)
        .map_err(|_| anyhow!("parse range error"))
}

fn get_s3_client() -> Result<Arc<Context>> {
    let s3_config_path = dirs::home_dir().unwrap().join(".s3config");
    let s3_config: S3Config = serde_json::from_reader(std::fs::File::open(s3_config_path)?)?;
    let part_size: u64 = s3_config.part_size;

    let path = dirs::home_dir().unwrap().join("datastore.json");

    let mut builder = SdkConfig::builder()
        .endpoint_url(&s3_config.endpoint)
        .region(Region::new(s3_config.region.clone()));

    if let (Some(ak), Some(sk)) = (&s3_config.access_key, &s3_config.secret_key) {
        builder = builder.credentials_provider(
            SharedCredentialsProvider::new(
                Credentials::new(ak, sk, None, None, "Static",)
            )
        )
    }

    let s3client = Client::new(&builder.build());

    let ctx = Context {
        part_size,
        datastore: std::sync::RwLock::new(Datastore::new(&path)?),
        tasks: std::sync::Mutex::new(HashMap::new()),
        s3client,
        s3config: s3_config,
    };

    Ok(Arc::new(ctx))
}