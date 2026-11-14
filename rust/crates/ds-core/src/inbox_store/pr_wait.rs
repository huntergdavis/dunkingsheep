//! Durable one-shot PR interests and pending event delivery through the normal inbox path.
use super::*;
use serde_json::{json, Value};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS pr_wait_http(url TEXT PRIMARY KEY, etag TEXT NOT NULL, body BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS pr_watches(repo TEXT NOT NULL, pr INTEGER NOT NULL, next_at REAL NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0, last_at REAL, PRIMARY KEY(repo,pr));
CREATE TABLE IF NOT EXISTS pr_waits(id INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, pr INTEGER NOT NULL, mailbox TEXT NOT NULL, pane TEXT NOT NULL, head TEXT, created_at REAL NOT NULL, event TEXT, UNIQUE(repo,pr,mailbox));";

#[derive(Clone, Debug)]
pub(crate) struct Watch {
    pub repo: String,
    pub pr: i64,
    pub failures: u32,
}
#[derive(Clone, Debug)]
pub(crate) struct Wait {
    pub id: i64,
    pub head: Option<String>,
}

impl Store {
    pub(crate) fn pr_wait_http_get(
        &self,
        url: &str,
    ) -> Result<Option<(String, Vec<u8>)>, DunkError> {
        let url = url.to_owned();
        self.writer.call(move |c| {
            c.query_row(
                "SELECT etag,body FROM pr_wait_http WHERE url=?",
                [url],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite)
        })
    }
    pub(crate) fn pr_wait_http_put(
        &self,
        url: String,
        etag: String,
        body: Vec<u8>,
    ) -> Result<(), DunkError> {
        self.writer.call(move |c| {
            c.execute("INSERT OR REPLACE INTO pr_wait_http(url,etag,body) VALUES(?,?,?)",rusqlite::params![url,etag,body]).map_err(map_sqlite)?;
            // A fixed storage bound, independent of the number of historical registrations.
            c.execute("DELETE FROM pr_wait_http WHERE rowid NOT IN(SELECT rowid FROM pr_wait_http ORDER BY rowid DESC LIMIT 100)",[]).map_err(map_sqlite)?;
            Ok(())
        })
    }
    pub(crate) fn pr_wait_register(
        &self,
        repo: &str,
        pr: i64,
        mailbox: &str,
        pane: &str,
        head: Option<&str>,
    ) -> Result<Value, DunkError> {
        let (repo, mailbox, pane, head) = (
            repo.to_owned(),
            mailbox.to_owned(),
            pane.to_owned(),
            head.map(str::to_owned),
        );
        self.writer.call(move |c| {
            let tx = c.transaction().map_err(map_sqlite)?;
            let watched: i64 = tx.query_row("SELECT COUNT(*) FROM (SELECT DISTINCT repo,pr FROM pr_waits)",[],|r|r.get(0)).map_err(map_sqlite)?;
            let shared: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pr_waits WHERE repo=? AND pr=?)",rusqlite::params![repo,pr],|r|r.get(0)).map_err(map_sqlite)?;
            if watched>=10 && !shared {return Err(refuse("at most 10 watched PRs globally; try again later"));}
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM pr_waits WHERE mailbox=?",[&mailbox],|r|r.get(0)).map_err(map_sqlite)?;
            let existing: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pr_waits WHERE repo=? AND pr=? AND mailbox=?)",rusqlite::params![repo,pr,mailbox],|r|r.get(0)).map_err(map_sqlite)?;
            if count>=20 && !existing {return Err(refuse("at most 20 PR waits per lane"));}
            if existing && head.is_some() {
                let pinned: Option<String> = tx.query_row("SELECT head FROM pr_waits WHERE repo=? AND pr=? AND mailbox=?",rusqlite::params![repo,pr,mailbox],|r|r.get(0)).map_err(map_sqlite)?;
                if pinned != head {return Err(refuse("active wait has a different head; cancel it before registering another head"));}
            }
            tx.execute("INSERT OR IGNORE INTO pr_watches(repo,pr) VALUES(?,?)", rusqlite::params![repo,pr]).map_err(map_sqlite)?;
            tx.execute("INSERT INTO pr_waits(repo,pr,mailbox,pane,head,created_at) VALUES(?,?,?,?,?,?) ON CONFLICT(repo,pr,mailbox) DO NOTHING", rusqlite::params![repo,pr,mailbox,pane,head,now_s()]).map_err(map_sqlite)?;
            let id: i64 = tx.query_row("SELECT id FROM pr_waits WHERE repo=? AND pr=? AND mailbox=?", rusqlite::params![repo,pr,mailbox], |r| r.get(0)).map_err(map_sqlite)?;
            tx.commit().map_err(map_sqlite)?;
            Ok(json!({"id":id,"repo":repo,"pr":pr,"registered":true}))
        })
    }
    pub(crate) fn pr_wait_list(
        &self,
        mailbox: &str,
        cancel: Option<i64>,
    ) -> Result<Value, DunkError> {
        let mailbox = mailbox.to_owned();
        self.writer.call(move |c| {
            if let Some(id) = cancel {
                let n = c.execute("DELETE FROM pr_waits WHERE id=? AND mailbox=?", rusqlite::params![id,mailbox]).map_err(map_sqlite)?;
                return Ok(json!({"cancelled":n}));
            }
            let mut s = c.prepare("SELECT w.id,w.repo,w.pr,w.head,p.last_at,p.next_at,w.created_at,w.event FROM pr_waits w JOIN pr_watches p ON p.repo=w.repo AND p.pr=w.pr WHERE mailbox=? ORDER BY w.id").map_err(map_sqlite)?;
            let rows = s.query_map([mailbox], |r| Ok(json!({"id":r.get::<_,i64>(0)?,"repo":r.get::<_,String>(1)?,"pr":r.get::<_,i64>(2)?,"head":r.get::<_,Option<String>>(3)?,"last_poll_at":r.get::<_,Option<f64>>(4)?,"next_due_at":r.get::<_,f64>(5)?,"expires_at":r.get::<_,f64>(6)?+86400.,"pending_notice":r.get::<_,Option<String>>(7)?}))).map_err(map_sqlite)?;
            Ok(Value::Array(rows.collect::<Result<Vec<_>,_>>().map_err(map_sqlite)?))
        })
    }
    /// A durable claim bounds concurrent/manual timer invocations to one stream per PR.
    pub(crate) fn pr_wait_due(&self, now: f64) -> Result<Vec<Watch>, DunkError> {
        self.writer.call(move |c| {
            let tx = c.transaction().map_err(map_sqlite)?;
            tx.execute("DELETE FROM pr_watches WHERE NOT EXISTS(SELECT 1 FROM pr_waits w WHERE w.repo=pr_watches.repo AND w.pr=pr_watches.pr)",[]).map_err(map_sqlite)?;
            let watches = {
                let mut s = tx.prepare("SELECT repo,pr,failures FROM pr_watches WHERE next_at<=? AND EXISTS(SELECT 1 FROM pr_waits w WHERE w.repo=pr_watches.repo AND w.pr=pr_watches.pr AND w.event IS NULL) ORDER BY next_at,repo,pr LIMIT 10").map_err(map_sqlite)?;
                let rows = s.query_map([now], |r| Ok(Watch{repo:r.get(0)?,pr:r.get(1)?,failures:r.get(2)?})).map_err(map_sqlite)?;
                rows.collect::<Result<Vec<_>,_>>().map_err(map_sqlite)?
            };
            for w in &watches { tx.execute("UPDATE pr_watches SET next_at=?,last_at=? WHERE repo=? AND pr=?",rusqlite::params![now+120.,now,w.repo,w.pr]).map_err(map_sqlite)?; }
            tx.commit().map_err(map_sqlite)?;
            Ok(watches)
        })
    }
    pub(crate) fn pr_wait_rows(&self, repo: &str, pr: i64) -> Result<Vec<Wait>, DunkError> {
        let repo = repo.to_owned();
        self.writer.call(move |c| {
            let mut s = c
                .prepare("SELECT id,head FROM pr_waits WHERE repo=? AND pr=? AND event IS NULL ORDER BY id")
                .map_err(map_sqlite)?;
            let rows = s
                .query_map(rusqlite::params![repo, pr], |r| {
                    Ok(Wait {
                        id: r.get(0)?,
                        head: r.get(1)?,
                    })
                })
                .map_err(map_sqlite)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
        })
    }
    pub(crate) fn pr_wait_head(&self, id: i64, head: &str) -> Result<(), DunkError> {
        let head = head.to_owned();
        self.writer.call(move |c| {
            c.execute(
                "UPDATE pr_waits SET head=? WHERE id=? AND head IS NULL",
                rusqlite::params![head, id],
            )
            .map_err(map_sqlite)?;
            Ok(())
        })
    }
    pub(crate) fn pr_wait_retry(
        &self,
        watch: &Watch,
        now: f64,
        failed: bool,
    ) -> Result<(), DunkError> {
        let w = watch.clone();
        self.writer.call(move |c| {
            let failures = if failed { w.failures + 1 } else { 0 };
            let delay = 120. * 2_f64.powi(failures.min(4) as i32);
            c.execute(
                "UPDATE pr_watches SET failures=?,next_at=? WHERE repo=? AND pr=?",
                rusqlite::params![failures, now + delay, w.repo, w.pr],
            )
            .map_err(map_sqlite)?;
            Ok(())
        })
    }
    /// Persist an ending event first; a crash before normal inbox_send retains this outbox.
    pub(crate) fn pr_wait_finish(&self, id: i64, body: &str) -> Result<(), DunkError> {
        let body = body.to_owned();
        self.writer.call(move |c| {
            c.execute(
                "UPDATE pr_waits SET event=? WHERE id=? AND event IS NULL",
                rusqlite::params![body, id],
            )
            .map_err(map_sqlite)?;
            Ok(())
        })
    }
    pub(crate) fn pr_wait_pending(
        &self,
        now: f64,
    ) -> Result<Vec<(i64, String, String)>, DunkError> {
        self.writer.call(move |c| {
            c.execute("UPDATE pr_waits SET event=repo || '#' || pr || ': wait expired after 24 hours' WHERE event IS NULL AND created_at<=?",[now-86400.]).map_err(map_sqlite)?;
            let mut s = c.prepare("SELECT id,mailbox,event FROM pr_waits WHERE event IS NOT NULL ORDER BY id").map_err(map_sqlite)?;
            let rows = s.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(map_sqlite)?;
            rows.collect::<Result<Vec<_>,_>>().map_err(map_sqlite)
        })
    }
    pub(crate) fn pr_wait_delivered(&self, id: i64) -> Result<(), DunkError> {
        self.writer.call(move |c| {
            c.execute(
                "DELETE FROM pr_waits WHERE id=? AND event IS NOT NULL",
                [id],
            )
            .map_err(map_sqlite)?;
            Ok(())
        })
    }
}
