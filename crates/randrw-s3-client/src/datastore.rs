use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Object {
    pub key: String,
    pub total_size: u64,
    pub part_size: u64,
    pub parts_len: u64,
}

pub struct Datastore {
    object_map: HashMap<String, Object>,
    datastore_file: File,
}

impl Datastore {
    pub fn new(path: &Path) -> Result<Datastore> {
        let out = if path.exists() {
            let datastore_file = OpenOptions::new().read(true).write(true).open(path)?;

            let object_map = serde_json::from_reader(&datastore_file)?;

            Datastore {
                datastore_file,
                object_map,
            }
        } else {
            let mut datastore_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(path)?;

            datastore_file.write_all(b"{}")?;

            Datastore {
                datastore_file,
                object_map: HashMap::new(),
            }
        };
        Ok(out)
    }

    pub fn put(&mut self, obj: Object) -> Result<()> {
        self.object_map.insert(obj.key.clone(), obj);
        self.datastore_file.seek(SeekFrom::Start(0))?;
        self.datastore_file.set_len(0)?;
        serde_json::to_writer(&self.datastore_file, &self.object_map)?;
        self.datastore_file.flush()?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<Object> {
        self.object_map.get(key).cloned()
    }
}
