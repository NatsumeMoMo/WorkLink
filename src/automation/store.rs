use super::{Budget, clip, now, permitted, valid_id};
use crate::config::Store;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

pub const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 48 * 1024;
pub const TTL: u64 = 24 * 3600;

#[derive(Clone)]
pub struct Results {
    pub store: Store,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_pages_search_ownership_expiry_and_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let cfg = store.init("result-test".into(), None).unwrap();
        let results = Results::new(store).unwrap();
        let text = "中文🙂 abc 中文🙂 end";
        let saved = results
            .save_text(&cfg.id, "browser", text, &Budget::default())
            .unwrap();
        let id = saved["result_id"].as_str().unwrap();
        let page = results.read(&cfg.id, id, 1, 3, None).unwrap();
        assert_eq!(page["text"], "文🙂 ");
        assert_eq!(page["next_offset"], 4);
        let found = results.read(&cfg.id, id, 0, 1, Some("中文")).unwrap();
        assert_eq!(found["matches"][0]["text"], "中");
        assert!(found["next_offset"].as_u64().unwrap() > 0);
        assert!(results.read("other-device", id, 0, 20, None).is_err());
        assert!(results.read(&cfg.id, id, 999, 20, None).is_err());
        let chunk = results.chunk(&cfg.id, id, 0).unwrap();
        assert_eq!(
            STANDARD.decode(chunk["data"].as_str().unwrap()).unwrap(),
            text.as_bytes()
        );
        let mut meta = results.metadata(&cfg.id, id).unwrap();
        meta.created = now() - TTL - 1;
        results
            .store
            .write_json(format!("automation/results/{id}.json"), &meta)
            .unwrap();
        assert!(results.read(&cfg.id, id, 0, 1, None).is_err());
    }
}
#[derive(Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub owner: String,
    pub capability: String,
    pub mime: String,
    pub size: usize,
    pub sha256: String,
    pub created: u64,
    pub source_truncated: bool,
}
impl Results {
    pub fn new(store: Store) -> Result<Self> {
        for path in [
            "automation/tasks",
            "automation/results",
            "automation/profiles",
        ] {
            fs::create_dir_all(store.root.join(path))?;
        }
        Ok(Self { store })
    }
    fn dir(&self) -> PathBuf {
        self.store.root.join("automation/results")
    }
    pub fn allocate(&self) -> (String, PathBuf) {
        let id = uuid::Uuid::new_v4().to_string();
        let path = self.dir().join(format!("{id}.bin"));
        (id, path)
    }
    pub fn save_bytes(
        &self,
        owner: &str,
        capability: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<Value> {
        ensure!(
            bytes.len() <= MAX_RESULT_BYTES,
            "result_too_large: 结果超过 2 MiB"
        );
        let (id, path) = self.allocate();
        fs::write(&path, bytes)?;
        self.finish(&id, owner, capability, mime, false)
    }
    pub fn finish(
        &self,
        id: &str,
        owner: &str,
        capability: &str,
        mime: &str,
        source_truncated: bool,
    ) -> Result<Value> {
        valid_id(id)?;
        let path = self.dir().join(format!("{id}.bin"));
        let bytes = fs::read(&path)?;
        ensure!(
            bytes.len() <= MAX_RESULT_BYTES,
            "result_too_large: 结果超过 2 MiB"
        );
        let meta = Meta {
            id: id.to_string(),
            owner: owner.to_string(),
            capability: capability.to_string(),
            mime: mime.to_string(),
            size: bytes.len(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            created: now(),
            source_truncated,
        };
        self.store
            .write_json(format!("automation/results/{id}.json"), &meta)?;
        self.prune()?;
        Ok(
            json!({"result_id":id,"mime":mime,"bytes":meta.size,"sha256":meta.sha256,"expires":meta.created+TTL,"source_truncated":source_truncated}),
        )
    }
    pub fn save_text(
        &self,
        owner: &str,
        capability: &str,
        text: &str,
        budget: &Budget,
    ) -> Result<Value> {
        let mut end = text.len().min(MAX_RESULT_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let truncated = end < text.len();
        let (id, path) = self.allocate();
        fs::write(path, &text.as_bytes()[..end])?;
        let mut value = self.finish(
            &id,
            owner,
            capability,
            "text/plain; charset=utf-8",
            truncated,
        )?;
        let text = &text[..end];
        let length = text.chars().count();
        let excerpt = clip(text, budget.text_chars());
        let offset = excerpt.chars().count();
        value["text"] = json!(excerpt);
        value["total_chars"] = json!(length);
        value["next_offset"] = if offset < length {
            json!(offset)
        } else {
            Value::Null
        };
        Ok(value)
    }
    pub fn metadata(&self, peer: &str, id: &str) -> Result<Meta> {
        valid_id(id)?;
        let meta: Meta = serde_json::from_slice(
            &fs::read(self.dir().join(format!("{id}.json")))
                .context("result_not_found: 结果已过期或不存在")?,
        )?;
        ensure!(meta.owner == peer, "result_not_found: 无权读取该结果");
        ensure!(
            now().saturating_sub(meta.created) <= TTL,
            "result_expired: 结果已过期"
        );
        permitted(&self.store, peer, &meta.capability)?;
        Ok(meta)
    }
    pub fn read(
        &self,
        peer: &str,
        id: &str,
        offset: usize,
        limit: usize,
        query: Option<&str>,
    ) -> Result<Value> {
        let meta = self.metadata(peer, id)?;
        ensure!(
            meta.mime.starts_with("text/"),
            "binary_result: 请使用 artifact 下载图片"
        );
        ensure!(
            (1..=16000).contains(&limit),
            "invalid_limit: limit 应为 1–16000"
        );
        let text = fs::read_to_string(self.dir().join(format!("{id}.bin")))?;
        let total = text.chars().count();
        ensure!(offset <= total, "invalid_offset: offset 超过结果长度");
        if let Some(query) = query {
            ensure!(
                !query.is_empty() && query.chars().count() <= 200,
                "invalid_query: query 应为 1–200 字符"
            );
            let start = text
                .char_indices()
                .nth(offset)
                .map_or(text.len(), |(i, _)| i);
            let mut matches = Vec::new();
            let mut used = 0;
            let mut next = None;
            for (index, _) in text[start..].match_indices(query).take(11) {
                let position = text[..start + index].chars().count();
                if matches.len() == 10 || used >= limit {
                    next = Some(position);
                    break;
                }
                let snippet: String = text
                    .chars()
                    .skip(position)
                    .take(240.min(limit - used))
                    .collect();
                let item = json!({"offset":position,"text":snippet});
                let cost = snippet.chars().count();
                used += cost;
                matches.push(item);
            }
            return Ok(
                json!({"result_id":id,"matches":matches,"next_offset":next,"total_chars":total}),
            );
        }
        let page: String = text.chars().skip(offset).take(limit).collect();
        let next = offset + page.chars().count();
        Ok(
            json!({"result_id":id,"text":page,"offset":offset,"next_offset":if next<total {Some(next)}else{None},"total_chars":total}),
        )
    }
    pub fn chunk(&self, peer: &str, id: &str, offset: u64) -> Result<Value> {
        let meta = self.metadata(peer, id)?;
        ensure!(
            offset <= meta.size as u64,
            "invalid_offset: offset 超过文件长度"
        );
        let mut file = fs::File::open(self.dir().join(format!("{id}.bin")))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0; CHUNK_BYTES.min(meta.size - offset as usize)];
        file.read_exact(&mut data)?;
        Ok(
            json!({"result_id":id,"offset":offset,"bytes":meta.size,"mime":meta.mime,"sha256":meta.sha256,"data":STANDARD.encode(&data),"next_offset":offset+data.len() as u64}),
        )
    }
    fn prune(&self) -> Result<()> {
        let mut items = vec![];
        for entry in fs::read_dir(self.dir())? {
            let path = entry?.path();
            if path.extension().is_some_and(|s| s == "json")
                && let Ok(meta) = serde_json::from_slice::<Meta>(&fs::read(&path)?)
            {
                items.push(meta);
            }
        }
        items.sort_by_key(|m| m.created);
        let mut bytes: usize = items.iter().map(|m| m.size).sum();
        let mut count = items.len();
        for meta in items {
            if now().saturating_sub(meta.created) > TTL || count > 200 || bytes > 128 * 1024 * 1024
            {
                for ext in ["json", "bin"] {
                    let _ = fs::remove_file(self.dir().join(format!("{}.{ext}", meta.id)));
                }
                bytes = bytes.saturating_sub(meta.size);
                count = count.saturating_sub(1);
            }
        }
        Ok(())
    }
}
