pub mod rocksdb;

use crate::waldecoder::{DecodedWALRecord, Oid, TransactionId, XlCreateDatabase, XlSmgrTruncate};
use crate::ZTimelineId;
use anyhow::Result;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use log::*;
use postgres_ffi::nonrelfile_utils::transaction_id_get_status;
use postgres_ffi::pg_constants;
use postgres_ffi::relfile_utils::forknumber_to_name;
use std::fmt;
use std::sync::Arc;
use zenith_utils::lsn::Lsn;

///
/// A repository corresponds to one .zenith directory. One repository holds multiple
/// timelines, forked off from the same initial call to 'initdb'.
pub trait Repository {
    /// Get Timeline handle for given zenith timeline ID.
    ///
    /// The Timeline is expected to be already "open", i.e. `get_or_restore_timeline`
    /// should've been called on it earlier already.
    fn get_timeline(&self, timelineid: ZTimelineId) -> Result<Arc<dyn Timeline>>;

    /// Get Timeline handle for given zenith timeline ID.
    ///
    /// Creates a new Timeline object if it's not "open" already.
    fn get_or_restore_timeline(&self, timelineid: ZTimelineId) -> Result<Arc<dyn Timeline>>;

    /// Create an empty timeline, without loading any data into it from possible on-disk snapshot.
    ///
    /// For unit tests.
    #[cfg(test)]
    fn create_empty_timeline(&self, timelineid: ZTimelineId) -> Result<Arc<dyn Timeline>>;

    //fn get_stats(&self) -> RepositoryStats;
}

pub trait Timeline {
    //------------------------------------------------------------------------------
    // Public GET functions
    //------------------------------------------------------------------------------

    /// Look up given page in the cache.
    fn get_page_at_lsn(&self, tag: BufferTag, lsn: Lsn) -> Result<Bytes>;

    /// Get size of relation
    fn get_relsize(&self, tag: RelTag, lsn: Lsn) -> Result<u32>;

    /// Does relation exist?
    fn get_relsize_exists(&self, tag: RelTag, lsn: Lsn) -> Result<bool>;

    /// Get page image at the particular LSN
    fn get_page_image(&self, tag: BufferTag, lsn: Lsn) -> Result<Option<Bytes>>;

    //------------------------------------------------------------------------------
    // Public PUT functions, to update the repository with new page versions.
    //
    // These are called by the WAL receiver to digest WAL records.
    //------------------------------------------------------------------------------

    /// Put a new page version that can be constructed from a WAL record
    ///
    /// This will implicitly extend the relation, if the page is beyond the
    /// current end-of-file.
    fn put_wal_record(&self, tag: BufferTag, rec: WALRecord) -> Result<()>;

    /// Like put_wal_record, but with ready-made image of the page.
    fn put_page_image(&self, tag: BufferTag, lsn: Lsn, img: Bytes) -> Result<()>;

    /// Truncate relation
    fn put_truncation(&self, rel: RelTag, lsn: Lsn, nblocks: u32) -> Result<()>;

    /// Drop relation or file segment
    fn put_drop(&self, tag: BufferTag, lsn: Lsn) -> Result<()>;

    /// Put raw data
    fn put_raw_data(&self, key: RepositoryKey, data: &[u8]) -> Result<()>;

    /// Get repository iterator
    fn iterator(&self) -> Box<dyn RepositoryIterator + '_>;

    /// Create a new database from a template database
    ///
    /// In PostgreSQL, CREATE DATABASE works by scanning the data directory and
    /// copying all relation files from the template database. This is the equivalent
    /// of that.
    fn put_create_database(
        &self,
        lsn: Lsn,
        db_id: Oid,
        tablespace_id: Oid,
        src_db_id: Oid,
        src_tablespace_id: Oid,
    ) -> Result<()> {
        let mut n = 0;
        for forknum in &[
            pg_constants::MAIN_FORKNUM,
            pg_constants::FSM_FORKNUM,
            pg_constants::VISIBILITYMAP_FORKNUM,
            pg_constants::INIT_FORKNUM,
            pg_constants::PG_FILENODEMAP_FORKNUM,
        ] {
            let key = RepositoryKey {
                tag: BufferTag {
                    rel: RelTag {
                        spcnode: src_tablespace_id,
                        dbnode: src_db_id,
                        relnode: 0,
                        forknum: *forknum,
                    },
                    blknum: 0,
                },
                lsn: Lsn(0),
            };
            let mut iter = self.iterator();
            iter.first(&key);
            while iter.valid() {
                let mut key = iter.key();
                if key.tag.rel.spcnode != src_tablespace_id || key.tag.rel.dbnode != src_db_id {
                    break;
                }
                key.tag.rel.spcnode = tablespace_id;
                key.tag.rel.dbnode = db_id;
                key.lsn = lsn;

                self.put_raw_data(key, iter.value())?;
                n += 1;
                iter.next();
            }
        }
        info!(
            "Create database {}/{}, copy {} entries",
            tablespace_id, db_id, n
        );
        Ok(())
    }

    ///
    /// Helper function to parse a WAL record and call the above functions for all the
    /// relations/pages that the record affects.
    ///
    fn save_decoded_record(
        &self,
        decoded: DecodedWALRecord,
        recdata: Bytes,
        lsn: Lsn,
    ) -> Result<()> {
        // Figure out which blocks the record applies to, and "put" a separate copy
        // of the record for each block.
        for blk in decoded.blocks.iter() {
            let tag = BufferTag {
                rel: RelTag {
                    spcnode: blk.rnode_spcnode,
                    dbnode: blk.rnode_dbnode,
                    relnode: blk.rnode_relnode,
                    forknum: blk.forknum as u8,
                },
                blknum: blk.blkno,
            };

            if blk.will_drop {
                self.put_drop(tag, lsn)?;
            } else {
                let rec = WALRecord {
                    lsn,
                    will_init: blk.will_init || blk.apply_image,
                    rec: recdata.clone(),
                    main_data_offset: decoded.main_data_offset as u32,
                };

                self.put_wal_record(tag, rec)?;
            }
        }

        // Handle a few special record types
        if decoded.xl_rmid == pg_constants::RM_SMGR_ID
            && (decoded.xl_info & pg_constants::XLR_RMGR_INFO_MASK)
                == pg_constants::XLOG_SMGR_TRUNCATE
        {
            let truncate = XlSmgrTruncate::decode(&decoded);
            if (truncate.flags & pg_constants::SMGR_TRUNCATE_HEAP) != 0 {
                let rel = RelTag {
                    spcnode: truncate.rnode.spcnode,
                    dbnode: truncate.rnode.dbnode,
                    relnode: truncate.rnode.relnode,
                    forknum: pg_constants::MAIN_FORKNUM,
                };
                self.put_truncation(rel, lsn, truncate.blkno)?;
            }
        } else if decoded.xl_rmid == pg_constants::RM_DBASE_ID
            && (decoded.xl_info & pg_constants::XLR_RMGR_INFO_MASK)
                == pg_constants::XLOG_DBASE_CREATE
        {
            let createdb = XlCreateDatabase::decode(&decoded);
            self.put_create_database(
                lsn,
                createdb.db_id,
                createdb.tablespace_id,
                createdb.src_db_id,
                createdb.src_tablespace_id,
            )?;
        }
        // Now that this record has been handled, let the repository know that
        // it is up-to-date to this LSN
        self.advance_last_record_lsn(lsn);
        Ok(())
    }

    /// Remember the all WAL before the given LSN has been processed.
    ///
    /// The WAL receiver calls this after the put_* functions, to indicate that
    /// all WAL before this point has been digested. Before that, if you call
    /// GET on an earlier LSN, it will block.
    fn advance_last_valid_lsn(&self, lsn: Lsn);
    fn get_last_valid_lsn(&self) -> Lsn;
    fn init_valid_lsn(&self, lsn: Lsn);

    /// Like `advance_last_valid_lsn`, but this always points to the end of
    /// a WAL record, not in the middle of one.
    ///
    /// This must be <= last valid LSN. This is tracked separately from last
    /// valid LSN, so that the WAL receiver knows where to restart streaming.
    fn advance_last_record_lsn(&self, lsn: Lsn);
    fn get_last_record_lsn(&self) -> Lsn;

    //
    // Wait until WAL has been received up to the given LSN.
    //
    fn wait_lsn(&self, lsn: Lsn) -> Result<Lsn>;

    /// Get range [begin,end) of stored blocks. Used mostly for SMGR pseudorelations
    /// but can be also applied to normal relations.
    fn get_range(&self, rel: RelTag, lsn: Lsn) -> Result<(u32, u32)> {
        let _lsn = self.wait_lsn(lsn)?;
        let mut key = RepositoryKey {
            // minimal key to start with
            tag: BufferTag { rel, blknum: 0 },
            lsn: Lsn(0),
        };
        let mut iter = self.iterator();
        iter.first(&key);
        if iter.valid() {
            let thiskey = iter.key();
            let tag = thiskey.tag;
            if tag.rel == rel {
                // still trversing this relation
                let first_blknum = tag.blknum;
                key.tag.blknum = u32::MAX; // maximal key
                iter.last(&key); // locate last entry
                if iter.valid() {
                    let thiskey = iter.key();
                    let last_blknum = thiskey.tag.blknum;
                    return Ok((first_blknum, last_blknum + 1)); // upper boundary is exclusive
                }
            }
        }
        Ok((0, 0)) // empty range
    }

    /// Get vector of databases (represented using RelTag only dbnode and spcnode fields are used)
    fn get_databases(&self, lsn: Lsn) -> Result<Vec<RelTag>> {
        let key = RepositoryKey {
            // minimal key
            tag: BufferTag {
                rel: RelTag {
                    forknum: pg_constants::PG_FILENODEMAP_FORKNUM,
                    spcnode: 0,
                    dbnode: 0,
                    relnode: 0,
                },
                blknum: 0,
            },
            lsn: Lsn(0),
        };
        let mut dbs = Vec::new();

        let mut iter = self.iterator();
        iter.first(&key);
        let mut prev_tag = key.tag.rel;
        while iter.valid() {
            let key = iter.key();
            if key.tag.rel.forknum != pg_constants::PG_FILENODEMAP_FORKNUM {
                break; // we are done with this fork
            }
            if key.tag.rel != prev_tag && key.lsn <= lsn {
                prev_tag = key.tag.rel;
                dbs.push(prev_tag); // collect unique tags
            }
            iter.next();
        }
        return Ok(dbs);
    }

    fn get_tx_status(&self, xid: TransactionId, lsn: Lsn) -> Result<u8> {
        let tag = BufferTag {
            rel: RelTag {
                forknum: pg_constants::PG_XACT_FORKNUM,
                spcnode: 0,
                dbnode: 0,
                relnode: 0,
            },
            blknum: xid / pg_constants::CLOG_XACTS_PER_PAGE,
        };
        let clog_page = self.get_page_at_lsn(tag, lsn)?;
        let status = transaction_id_get_status(xid, &clog_page[..]);
        Ok(status)
    }

    /// Get vector of prepared twophase transactions
    fn get_twophase(&self, lsn: Lsn) -> Result<Vec<TransactionId>> {
        let key = RepositoryKey {
            // minimal key
            tag: BufferTag {
                rel: RelTag {
                    forknum: pg_constants::PG_TWOPHASE_FORKNUM,
                    spcnode: 0,
                    dbnode: 0,
                    relnode: 0,
                },
                blknum: 0,
            },
            lsn: Lsn(0),
        };
        let mut gxacts = Vec::new();

        let mut iter = self.iterator();
        iter.first(&key);
        while iter.valid() {
            let key = iter.key();
            if key.tag.rel.forknum != pg_constants::PG_TWOPHASE_FORKNUM {
                break; // we are done with this fork
            }
            if key.lsn <= lsn {
                let xid = key.tag.blknum;
                if self.get_tx_status(xid, lsn)? == pg_constants::TRANSACTION_STATUS_IN_PROGRESS {
                    gxacts.push(xid);
                }
            }
            iter.next();
        }
        return Ok(gxacts);
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct RepositoryKey {
    pub tag: BufferTag,
    pub lsn: Lsn,
}

impl RepositoryKey {
    fn pack(&self, buf: &mut BytesMut) {
        self.tag.pack(buf);
        buf.put_u64(self.lsn.0);
    }
    fn unpack(buf: &mut Bytes) -> RepositoryKey {
        RepositoryKey {
            tag: BufferTag::unpack(buf),
            lsn: Lsn::from(buf.get_u64()),
        }
    }

    fn from_slice(slice: &[u8]) -> Self {
        let mut buf = Bytes::copy_from_slice(slice);
        Self::unpack(&mut buf)
    }

    fn to_bytes(&self) -> BytesMut {
        let mut buf = BytesMut::new();
        self.pack(&mut buf);
        buf
    }
}

pub trait RepositoryIterator {
    fn first(&mut self, key: &RepositoryKey);
    fn last(&mut self, key: &RepositoryKey);
    fn next(&mut self);
    fn prev(&mut self);
    fn valid(&self) -> bool;
    fn key(&self) -> RepositoryKey;
    fn value(&self) -> &[u8];
}

#[derive(Clone)]
pub struct RepositoryStats {
    pub num_entries: Lsn,
    pub num_page_images: Lsn,
    pub num_wal_records: Lsn,
    pub num_getpage_requests: Lsn,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Hash, Ord, Clone, Copy)]
pub struct RelTag {
    pub forknum: u8,
    pub spcnode: u32,
    pub dbnode: u32,
    pub relnode: u32,
}

impl RelTag {
    pub fn pack(&self, buf: &mut BytesMut) {
        buf.put_u8(self.forknum);
        buf.put_u32(self.spcnode);
        buf.put_u32(self.dbnode);
        buf.put_u32(self.relnode);
    }
    pub fn unpack(buf: &mut Bytes) -> RelTag {
        RelTag {
            forknum: buf.get_u8(),
            spcnode: buf.get_u32(),
            dbnode: buf.get_u32(),
            relnode: buf.get_u32(),
        }
    }
}

/// Display RelTag in the same format that's used in most PostgreSQL debug messages:
///
/// <spcnode>/<dbnode>/<relnode>[_fsm|_vm|_init]
///
impl fmt::Display for RelTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(forkname) = forknumber_to_name(self.forknum) {
            write!(
                f,
                "{}/{}/{}_{}",
                self.spcnode, self.dbnode, self.relnode, forkname
            )
        } else {
            write!(f, "{}/{}/{}", self.spcnode, self.dbnode, self.relnode)
        }
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub struct BufferTag {
    pub rel: RelTag,
    pub blknum: u32,
}

impl BufferTag {
    pub fn fork(forknum: u8) -> BufferTag {
        BufferTag {
            rel: RelTag {
                forknum,
                spcnode: 0,
                dbnode: 0,
                relnode: 0,
            },
            blknum: 0,
        }
    }

    pub fn pack(&self, buf: &mut BytesMut) {
        self.rel.pack(buf);
        buf.put_u32(self.blknum);
    }
    pub fn unpack(buf: &mut Bytes) -> BufferTag {
        BufferTag {
            rel: RelTag::unpack(buf),
            blknum: buf.get_u32(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WALRecord {
    pub lsn: Lsn, // LSN at the *end* of the record
    pub will_init: bool,
    pub rec: Bytes,
    // Remember the offset of main_data in rec,
    // so that we don't have to parse the record again.
    // If record has no main_data, this offset equals rec.len().
    pub main_data_offset: u32,
}

impl WALRecord {
    pub fn pack(&self, buf: &mut BytesMut) {
        buf.put_u64(self.lsn.0);
        buf.put_u8(self.will_init as u8);
        buf.put_u32(self.main_data_offset);
        buf.put_u32(self.rec.len() as u32);
        buf.put_slice(&self.rec[..]);
    }
    pub fn unpack(buf: &mut Bytes) -> WALRecord {
        let lsn = Lsn::from(buf.get_u64());
        let will_init = buf.get_u8() != 0;
        let main_data_offset = buf.get_u32();
        let mut dst = vec![0u8; buf.get_u32() as usize];
        buf.copy_to_slice(&mut dst);
        WALRecord {
            lsn,
            will_init,
            rec: Bytes::from(dst),
            main_data_offset,
        }
    }
}

///
/// Tests that should work the same with any Repository/Timeline implementation.
///
#[cfg(test)]
mod tests {
    use super::*;
    use crate::walredo::{WalRedoError, WalRedoManager};
    use crate::PageServerConf;
    use postgres_ffi::pg_constants;
    use std::fs;
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::time::Duration;

    /// Arbitrary relation tag, for testing.
    const TESTREL_A: RelTag = RelTag {
        spcnode: 0,
        dbnode: 111,
        relnode: 1000,
        forknum: 0,
    };

    /// Convenience function to create a BufferTag for testing.
    /// Helps to keeps the tests shorter.
    #[allow(non_snake_case)]
    fn TEST_BUF(blknum: u32) -> BufferTag {
        BufferTag {
            rel: TESTREL_A,
            blknum,
        }
    }

    /// Convenience function to create a page image with given string as the only content
    #[allow(non_snake_case)]
    fn TEST_IMG(s: &str) -> Bytes {
        let mut buf = BytesMut::new();
        buf.extend_from_slice(s.as_bytes());
        buf.resize(8192, 0);

        buf.freeze()
    }

    fn get_test_repo(test_name: &str) -> Result<Box<dyn Repository>> {
        let repo_dir = PathBuf::from(format!("../tmp_check/test_{}", test_name));
        let _ = fs::remove_dir_all(&repo_dir);
        fs::create_dir_all(&repo_dir)?;

        let conf = PageServerConf {
            daemonize: false,
            interactive: false,
            gc_horizon: 64 * 1024 * 1024,
            gc_period: Duration::from_secs(10),
            listen_addr: "127.0.0.1:5430".parse().unwrap(),
            workdir: repo_dir,
            pg_distrib_dir: "".into(),
        };
        // Make a static copy of the config. This can never be free'd, but that's
        // OK in a test.
        let conf: &'static PageServerConf = Box::leak(Box::new(conf));

        let walredo_mgr = TestRedoManager {};

        let repo = rocksdb::RocksRepository::new(conf, Arc::new(walredo_mgr));

        Ok(Box::new(repo))
    }

    /// Test get_relsize() and truncation.
    #[test]
    fn test_relsize() -> Result<()> {
        // get_timeline() with non-existent timeline id should fail
        //repo.get_timeline("11223344556677881122334455667788");

        // Create timeline to work on
        let repo = get_test_repo("test_relsize")?;
        let timelineid = ZTimelineId::from_str("11223344556677881122334455667788").unwrap();
        let tline = repo.create_empty_timeline(timelineid)?;

        tline.init_valid_lsn(Lsn(1));
        tline.put_page_image(TEST_BUF(0), Lsn(2), TEST_IMG("foo blk 0 at 2"))?;
        tline.put_page_image(TEST_BUF(0), Lsn(2), TEST_IMG("foo blk 0 at 2"))?;
        tline.put_page_image(TEST_BUF(0), Lsn(3), TEST_IMG("foo blk 0 at 3"))?;
        tline.put_page_image(TEST_BUF(1), Lsn(4), TEST_IMG("foo blk 1 at 4"))?;
        tline.put_page_image(TEST_BUF(2), Lsn(5), TEST_IMG("foo blk 2 at 5"))?;

        tline.advance_last_valid_lsn(Lsn(5));

        // FIXME: The rocksdb implementation erroneously returns 'true' here, even
        // though the relation was created only at a later LSN
        // rocksdb implementation erroneosly returns 'true' here
        assert_eq!(tline.get_relsize_exists(TESTREL_A, Lsn(1))?, true); // CORRECT: false
                                                                        // And this probably should throw an error, becaue the relation doesn't exist at Lsn(1) yet
        assert_eq!(tline.get_relsize(TESTREL_A, Lsn(1))?, 0); // CORRECT: throw error

        assert_eq!(tline.get_relsize_exists(TESTREL_A, Lsn(2))?, true);
        assert_eq!(tline.get_relsize(TESTREL_A, Lsn(2))?, 1);
        assert_eq!(tline.get_relsize(TESTREL_A, Lsn(5))?, 3);

        // Check page contents at each LSN
        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(0), Lsn(2))?,
            TEST_IMG("foo blk 0 at 2")
        );

        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(0), Lsn(3))?,
            TEST_IMG("foo blk 0 at 3")
        );

        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(0), Lsn(4))?,
            TEST_IMG("foo blk 0 at 3")
        );
        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(1), Lsn(4))?,
            TEST_IMG("foo blk 1 at 4")
        );

        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(0), Lsn(5))?,
            TEST_IMG("foo blk 0 at 3")
        );
        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(1), Lsn(5))?,
            TEST_IMG("foo blk 1 at 4")
        );
        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(2), Lsn(5))?,
            TEST_IMG("foo blk 2 at 5")
        );

        // Truncate last block
        tline.put_truncation(TESTREL_A, Lsn(6), 2)?;
        tline.advance_last_valid_lsn(Lsn(6));

        // Check reported size and contents after truncation
        assert_eq!(tline.get_relsize(TESTREL_A, Lsn(6))?, 2);
        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(0), Lsn(6))?,
            TEST_IMG("foo blk 0 at 3")
        );
        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(1), Lsn(6))?,
            TEST_IMG("foo blk 1 at 4")
        );

        // should still see the truncated block with older LSN
        assert_eq!(tline.get_relsize(TESTREL_A, Lsn(5))?, 3);
        assert_eq!(
            tline.get_page_at_lsn(TEST_BUF(2), Lsn(5))?,
            TEST_IMG("foo blk 2 at 5")
        );

        Ok(())
    }

    /// Test get_relsize() and truncation with a file larger than 1 GB, so that it's
    /// split into multiple 1 GB segments in Postgres.
    ///
    /// This isn't very interesting with the RocksDb implementation, as we don't pay
    /// any attention to Postgres segment boundaries there.
    #[test]
    fn test_large_rel() -> Result<()> {
        let repo = get_test_repo("test_large_rel")?;
        let timelineid = ZTimelineId::from_str("11223344556677881122334455667788").unwrap();
        let tline = repo.create_empty_timeline(timelineid)?;

        tline.init_valid_lsn(Lsn(1));

        let mut lsn = 0;
        for i in 0..pg_constants::RELSEG_SIZE + 1 {
            let img = TEST_IMG(&format!("foo blk {} at {}", i, Lsn(lsn)));
            lsn += 1;
            tline.put_page_image(TEST_BUF(i as u32), Lsn(lsn), img)?;
        }
        tline.advance_last_valid_lsn(Lsn(lsn));

        assert_eq!(
            tline.get_relsize(TESTREL_A, Lsn(lsn))?,
            pg_constants::RELSEG_SIZE + 1
        );

        // Truncate one block
        lsn += 1;
        tline.put_truncation(TESTREL_A, Lsn(lsn), pg_constants::RELSEG_SIZE)?;
        tline.advance_last_valid_lsn(Lsn(lsn));
        assert_eq!(
            tline.get_relsize(TESTREL_A, Lsn(lsn))?,
            pg_constants::RELSEG_SIZE
        );

        // Truncate another block
        lsn += 1;
        tline.put_truncation(TESTREL_A, Lsn(lsn), pg_constants::RELSEG_SIZE - 1)?;
        tline.advance_last_valid_lsn(Lsn(lsn));
        assert_eq!(
            tline.get_relsize(TESTREL_A, Lsn(lsn))?,
            pg_constants::RELSEG_SIZE - 1
        );

        Ok(())
    }

    // Mock WAL redo manager that doesn't do much
    struct TestRedoManager {}

    impl WalRedoManager for TestRedoManager {
        fn request_redo(
            &self,
            tag: BufferTag,
            lsn: Lsn,
            base_img: Option<Bytes>,
            records: Vec<WALRecord>,
        ) -> Result<Bytes, WalRedoError> {
            let s = format!(
                "redo for rel {} blk {} to get to {}, with {} and {} records",
                tag.rel,
                tag.blknum,
                lsn,
                if base_img.is_some() {
                    "base image"
                } else {
                    "no base image"
                },
                records.len()
            );
            println!("{}", s);
            Ok(TEST_IMG(&s))
        }
    }
}
