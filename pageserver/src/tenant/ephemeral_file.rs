//! Implementation of append-only file data structure
//! used to keep in-memory layers spilled on disk.

use crate::config::PageServerConf;
use crate::context::RequestContext;
use crate::page_cache;
use crate::tenant::block_io::{BlockCursor, BlockLease, BlockReader};
use crate::virtual_file::{self, VirtualFile};
use camino::Utf8PathBuf;
use pageserver_api::shard::TenantShardId;

use std::io;
use std::sync::atomic::AtomicU64;
use utils::id::TimelineId;

pub struct EphemeralFile {
    _tenant_shard_id: TenantShardId,
    _timeline_id: TimelineId,

    rw: page_caching::RW,
}

mod page_caching;
mod zero_padded_read_write;

impl EphemeralFile {
    pub async fn create(
        conf: &PageServerConf,
        tenant_shard_id: TenantShardId,
        timeline_id: TimelineId,
        gate_guard: utils::sync::gate::GateGuard,
        ctx: &RequestContext,
    ) -> Result<EphemeralFile, io::Error> {
        static NEXT_FILENAME: AtomicU64 = AtomicU64::new(1);
        let filename_disambiguator =
            NEXT_FILENAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let filename = conf
            .timeline_path(&tenant_shard_id, &timeline_id)
            .join(Utf8PathBuf::from(format!(
                "ephemeral-{filename_disambiguator}"
            )));

        let file = VirtualFile::open_with_options(
            &filename,
            virtual_file::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true),
            ctx,
        )
        .await?;

        Ok(EphemeralFile {
            _tenant_shard_id: tenant_shard_id,
            _timeline_id: timeline_id,
            rw: page_caching::RW::new(file, gate_guard),
        })
    }

    pub(crate) fn len(&self) -> u64 {
        self.rw.bytes_written()
    }

    pub(crate) fn page_cache_file_id(&self) -> page_cache::FileId {
        self.rw.page_cache_file_id()
    }

    /// See [`self::page_caching::RW::load_to_vec`].
    pub(crate) async fn load_to_vec(&self, ctx: &RequestContext) -> Result<Vec<u8>, io::Error> {
        self.rw.load_to_vec(ctx).await
    }

    pub(crate) async fn read_blk(
        &self,
        blknum: u32,
        ctx: &RequestContext,
    ) -> Result<BlockLease, io::Error> {
        self.rw.read_blk(blknum, ctx).await
    }

    #[cfg(test)]
    // This is a test helper: outside of tests, we are always written to via a pre-serialized batch.
    pub(crate) async fn write_blob(
        &mut self,
        srcbuf: &[u8],
        ctx: &RequestContext,
    ) -> Result<u64, io::Error> {
        let pos = self.rw.bytes_written();

        let mut len_bytes = std::io::Cursor::new(Vec::new());
        crate::tenant::storage_layer::inmemory_layer::SerializedBatch::write_blob_length(
            srcbuf.len(),
            &mut len_bytes,
        );
        let len_bytes = len_bytes.into_inner();

        // Write the length field
        self.rw.write_all_borrowed(&len_bytes, ctx).await?;

        // Write the payload
        self.rw.write_all_borrowed(srcbuf, ctx).await?;

        Ok(pos)
    }

    /// Returns the offset at which the first byte of the input was written, for use
    /// in constructing indices over the written value.
    pub(crate) async fn write_raw(
        &mut self,
        srcbuf: &[u8],
        ctx: &RequestContext,
    ) -> Result<u64, io::Error> {
        let pos = self.rw.bytes_written();

        // Write the payload
        self.rw.write_all_borrowed(srcbuf, ctx).await?;

        Ok(pos)
    }
}

/// Does the given filename look like an ephemeral file?
pub fn is_ephemeral_file(filename: &str) -> bool {
    if let Some(rest) = filename.strip_prefix("ephemeral-") {
        rest.parse::<u32>().is_ok()
    } else {
        false
    }
}

impl BlockReader for EphemeralFile {
    fn block_cursor(&self) -> super::block_io::BlockCursor<'_> {
        BlockCursor::new(super::block_io::BlockReaderRef::EphemeralFile(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::DownloadBehavior;
    use crate::task_mgr::TaskKind;
    use crate::tenant::block_io::BlockReaderRef;
    use rand::{thread_rng, RngCore};
    use std::fs;
    use std::str::FromStr;

    fn harness(
        test_name: &str,
    ) -> Result<
        (
            &'static PageServerConf,
            TenantShardId,
            TimelineId,
            RequestContext,
        ),
        io::Error,
    > {
        let repo_dir = PageServerConf::test_repo_dir(test_name);
        let _ = fs::remove_dir_all(&repo_dir);
        let conf = PageServerConf::dummy_conf(repo_dir);
        // Make a static copy of the config. This can never be free'd, but that's
        // OK in a test.
        let conf: &'static PageServerConf = Box::leak(Box::new(conf));

        let tenant_shard_id = TenantShardId::from_str("11000000000000000000000000000000").unwrap();
        let timeline_id = TimelineId::from_str("22000000000000000000000000000000").unwrap();
        fs::create_dir_all(conf.timeline_path(&tenant_shard_id, &timeline_id))?;

        let ctx = RequestContext::new(TaskKind::UnitTest, DownloadBehavior::Error);

        Ok((conf, tenant_shard_id, timeline_id, ctx))
    }

    #[tokio::test]
    async fn test_ephemeral_blobs() -> Result<(), io::Error> {
        let (conf, tenant_id, timeline_id, ctx) = harness("ephemeral_blobs")?;

        let gate = utils::sync::gate::Gate::default();

        let entered = gate.enter().unwrap();

        let mut file = EphemeralFile::create(conf, tenant_id, timeline_id, entered, &ctx).await?;

        let pos_foo = file.write_blob(b"foo", &ctx).await?;
        assert_eq!(
            b"foo",
            file.block_cursor()
                .read_blob(pos_foo, &ctx)
                .await?
                .as_slice()
        );
        let pos_bar = file.write_blob(b"bar", &ctx).await?;
        assert_eq!(
            b"foo",
            file.block_cursor()
                .read_blob(pos_foo, &ctx)
                .await?
                .as_slice()
        );
        assert_eq!(
            b"bar",
            file.block_cursor()
                .read_blob(pos_bar, &ctx)
                .await?
                .as_slice()
        );

        let mut blobs = Vec::new();
        for i in 0..10000 {
            let data = Vec::from(format!("blob{}", i).as_bytes());
            let pos = file.write_blob(&data, &ctx).await?;
            blobs.push((pos, data));
        }
        // also test with a large blobs
        for i in 0..100 {
            let data = format!("blob{}", i).as_bytes().repeat(100);
            let pos = file.write_blob(&data, &ctx).await?;
            blobs.push((pos, data));
        }

        let cursor = BlockCursor::new(BlockReaderRef::EphemeralFile(&file));
        for (pos, expected) in blobs {
            let actual = cursor.read_blob(pos, &ctx).await?;
            assert_eq!(actual, expected);
        }

        // Test a large blob that spans multiple pages
        let mut large_data = vec![0; 20000];
        thread_rng().fill_bytes(&mut large_data);
        let pos_large = file.write_blob(&large_data, &ctx).await?;
        let result = file.block_cursor().read_blob(pos_large, &ctx).await?;
        assert_eq!(result, large_data);

        Ok(())
    }

    #[tokio::test]
    async fn ephemeral_file_holds_gate_open() {
        const FOREVER: std::time::Duration = std::time::Duration::from_secs(5);

        let (conf, tenant_id, timeline_id, ctx) =
            harness("ephemeral_file_holds_gate_open").unwrap();

        let gate = utils::sync::gate::Gate::default();

        let file = EphemeralFile::create(conf, tenant_id, timeline_id, gate.enter().unwrap(), &ctx)
            .await
            .unwrap();

        let mut closing = tokio::task::spawn(async move {
            gate.close().await;
        });

        // gate is entered until the ephemeral file is dropped
        // do not start paused tokio-epoll-uring has a sleep loop
        tokio::time::pause();
        tokio::time::timeout(FOREVER, &mut closing)
            .await
            .expect_err("closing cannot complete before dropping");

        // this is a requirement of the reset_tenant functionality: we have to be able to restart a
        // tenant fast, and for that, we need all tenant_dir operations be guarded by entering a gate
        drop(file);

        tokio::time::timeout(FOREVER, &mut closing)
            .await
            .expect("closing completes right away")
            .expect("closing does not panic");
    }
}
