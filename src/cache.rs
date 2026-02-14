use sled::Db;
use std::path::Path;

pub struct Cache {
    db: Db,
}

impl Cache {
    pub fn new<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let db = sled::open(path)?;
        Ok(Self { db })
    }

    pub async fn get(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(self.db.get(key)?.map(|v| v.to_vec()))
    }

    pub async fn put(&self, key: &str, value: &[u8]) -> anyhow::Result<()> {
        self.db.insert(key, value)?;
        self.db.flush_async().await?;
        Ok(())
    }

    pub async fn clear(&self) -> anyhow::Result<()> {
        self.db.clear()?;
        Ok(())
    }
}
