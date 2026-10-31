//! Contract suites of the message files: the behaviour every [`OutboxReader`] and every
//! [`InboxWriter`] must have. The in-memory fakes of [`World`] pass them
//! (`crates/fleet-app/tests/contracts_memory.rs`) and so does the real file adapter
//! (`crates/fleet-infra/tests/contracts.rs`).
//!
//! | suite                                    | ports                     | harness                |
//! |------------------------------------------|---------------------------|------------------------|
//! | [`outbox_contract`], [`inbox_contract`]  | `OutboxReader`, `InboxWriter` | `&mut dyn MailHarness` |
//!
//! The suite builds worktree content only through the harness, so the harness decides how a link
//! or a FIFO is made on its medium. Planted content that is not valid carries [`PLANT_MARKER`],
//! so the suite can check that error texts do not echo what a worker wrote.

use super::contracts::PLANT_MARKER;
use super::World;
use crate::ports::{InboxWriter, OutboxRead, OutboxReader};
use fleet_domain::ids::{ChannelName, MessageId, TaskId};
use fleet_domain::msg::inbox::{render_manifest, InboxItem, INDEX_FILE};
use fleet_domain::msg::message::OutboxMessage;
use fleet_domain::status::{BlockReason, MessageKind};
use std::collections::BTreeMap;

/// What the worker leaves in `.fleet/outbox/<name>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboxPlant {
    /// A well-formed message to task `b` on channel `talk`.
    Valid,
    /// Text that is not JSON (carrying [`PLANT_MARKER`]).
    Malformed,
    /// A file larger than the adapter reads.
    Oversized,
    /// A link to a file that holds a valid message elsewhere.
    Link,
    /// A FIFO nobody writes to.
    Fifo,
    /// A directory.
    Directory,
}

/// The medium the outbox reader and the inbox writer work on, from the suite's point of view.
pub trait MailHarness {
    fn outbox(&self) -> &dyn OutboxReader;
    fn inbox(&self) -> &dyn InboxWriter;
    /// The worktree path passed to the adapters. `.fleet` exists; nothing else does.
    fn worktree(&self) -> String;
    /// Plants `.fleet/outbox/<name>` as `what`, creating the directory when needed.
    fn plant_outbox(&mut self, name: &str, what: OutboxPlant);
    /// The files still in the outbox (not settled), by name.
    fn outbox_names(&self) -> Vec<String>;
    /// Replaces `.fleet/outbox` by a link to another directory.
    fn replace_outbox_with_link(&mut self);
    /// The files in `.fleet/inbox`, name to text.
    fn inbox_files(&self) -> BTreeMap<String, String>;
    /// The worker makes a file in its own inbox, as if it were a delivery.
    fn plant_inbox_stray(&mut self, name: &str);
    /// The worker replaces `.fleet/inbox` by a link to another directory, which must stay empty.
    fn replace_inbox_with_link(&mut self);
    /// Whether the directory the inbox link points at is still empty (nothing leaked through).
    fn link_target_untouched(&self) -> bool;
}

/// [`MailHarness`] over the in-memory fakes of a [`World`].
pub struct MemoryMailHarness {
    world: World,
    outbox: Box<dyn OutboxReader>,
    inbox: Box<dyn InboxWriter>,
    worktree: String,
}

impl MemoryMailHarness {
    pub fn new(world: World) -> MemoryMailHarness {
        let ports = world.ports();
        MemoryMailHarness {
            outbox: ports.outbox,
            inbox: ports.inbox,
            world,
            worktree: "/work/mail".to_string(),
        }
    }
}

impl MailHarness for MemoryMailHarness {
    fn outbox(&self) -> &dyn OutboxReader {
        self.outbox.as_ref()
    }
    fn inbox(&self) -> &dyn InboxWriter {
        self.inbox.as_ref()
    }
    fn worktree(&self) -> String {
        self.worktree.clone()
    }
    fn plant_outbox(&mut self, name: &str, what: OutboxPlant) {
        let read = match what {
            OutboxPlant::Valid => OutboxRead::Valid(OutboxMessage {
                channel: ChannelName::parse("talk").unwrap(),
                to: Some(TaskId::parse("b").unwrap()),
                kind: MessageKind::Info,
                subject: "s".to_string(),
                body: "b".to_string(),
                in_reply_to: None,
            }),
            OutboxPlant::Oversized => {
                OutboxRead::Invalid(BlockReason::TooLarge, "the file is too large".to_string())
            }
            other => OutboxRead::Invalid(BlockReason::Invalid, format!("planted {other:?} file")),
        };
        self.world.write_outbox(&self.worktree, name, read);
    }
    fn outbox_names(&self) -> Vec<String> {
        self.world.outbox_names(&self.worktree)
    }
    fn replace_outbox_with_link(&mut self) {
        self.world
            .refuse_outbox(&self.worktree, "the outbox is a link or not a directory");
    }
    fn inbox_files(&self) -> BTreeMap<String, String> {
        let items = self.world.inbox(&self.worktree);
        let mut files: BTreeMap<String, String> = items
            .iter()
            .map(|i| (i.file_name(), i.content.clone()))
            .collect();
        files.insert(INDEX_FILE.to_string(), render_manifest(&items));
        files
    }
    fn plant_inbox_stray(&mut self, name: &str) {
        self.world.plant_inbox_stray(&self.worktree, name);
    }
    fn replace_inbox_with_link(&mut self) {
        self.world.fail_inbox(true);
    }
    fn link_target_untouched(&self) -> bool {
        true
    }
}

fn mail_item(n: u64, text: &str) -> InboxItem {
    InboxItem {
        id: MessageId::from_seq(n),
        seq: n,
        content: format!("{text}\n"),
        sha256: format!("{n:064x}"),
    }
}

/// The behaviour every [`OutboxReader`] must have: whatever a worker plants, `scan` returns at
/// once with something safe to record, never follows a link and never reads more than it should.
pub fn outbox_contract(h: &mut dyn MailHarness) {
    let wt = h.worktree();
    // nothing planted: nothing to read, and no complaint
    let scan = h.outbox().scan(&wt, 8);
    assert!(
        scan.items.is_empty() && scan.refused.is_none(),
        "outbox contract: a missing outbox is empty"
    );

    h.plant_outbox("10.json", OutboxPlant::Valid);
    h.plant_outbox("2.json", OutboxPlant::Valid);
    let scan = h.outbox().scan(&wt, 8);
    let names: Vec<&str> = scan.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        ["2.json", "10.json"],
        "outbox contract: files come in numeric order"
    );
    assert!(
        scan.items
            .iter()
            .all(|i| matches!(i.read, OutboxRead::Valid(_))),
        "outbox contract: valid files read as valid"
    );
    assert_eq!(
        h.outbox().scan(&wt, 1).items.len(),
        1,
        "outbox contract: at most `max` files per scan"
    );

    // everything hostile is Invalid, named, and does not echo the planted content
    for (name, what) in [
        ("20.json", OutboxPlant::Malformed),
        ("21.json", OutboxPlant::Oversized),
        ("22.json", OutboxPlant::Link),
        ("23.json", OutboxPlant::Fifo),
        ("24.json", OutboxPlant::Directory),
    ] {
        h.plant_outbox(name, what);
        let scan = h.outbox().scan(&wt, 16);
        let item = scan
            .items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("outbox contract: {name} ({what:?}) was not reported"));
        match &item.read {
            OutboxRead::Invalid(_, why) => assert!(
                !why.contains(PLANT_MARKER),
                "outbox contract: the error for {what:?} must not echo the file: {why}"
            ),
            other => panic!("outbox contract: {what:?} must be Invalid, got {other:?}"),
        }
        if what == OutboxPlant::Oversized {
            assert!(
                matches!(item.read, OutboxRead::Invalid(BlockReason::TooLarge, _)),
                "outbox contract: an oversized file is `too-large`"
            );
        }
    }

    // settling: an accepted file is gone; a refused one is not read again either
    h.outbox().settle(&wt, "2.json", true).unwrap();
    h.outbox().settle(&wt, "10.json", false).unwrap();
    h.outbox().settle(&wt, "10.json", false).unwrap();
    let left = h.outbox_names();
    assert!(
        !left.contains(&"2.json".to_string()) && !left.contains(&"10.json".to_string()),
        "outbox contract: settled files are not read again: {left:?}"
    );
    assert!(
        h.outbox().settle(&wt, "../x.json", true).is_err(),
        "outbox contract: only outbox file names can be settled"
    );

    // a replaced outbox is refused as a whole, never followed
    h.replace_outbox_with_link();
    let scan = h.outbox().scan(&wt, 8);
    assert!(
        scan.items.is_empty() && scan.refused.is_some(),
        "outbox contract: an outbox that is a link is refused"
    );
}

/// The behaviour every [`InboxWriter`] must have: the inbox holds exactly what was delivered
/// (plus the index), writes are idempotent, files the daemon did not write are removed and
/// reported, and an inbox that was replaced by a link is never written through.
pub fn inbox_contract(h: &mut dyn MailHarness) {
    let wt = h.worktree();
    let one = mail_item(1, "one");
    let report = h
        .inbox()
        .prepare(&wt, std::slice::from_ref(&one))
        .expect("inbox contract: prepare");
    assert_eq!(report.written, vec![one.id.clone()]);
    let files = h.inbox_files();
    assert_eq!(files.get("m1.json"), Some(&one.content));
    assert!(
        files
            .get("index.json")
            .is_some_and(|i| i.contains("m1.json") && i.contains(&one.sha256)),
        "inbox contract: index.json lists every delivery with its digest"
    );

    // delivering again changes nothing
    let report = h.inbox().sync(&wt, std::slice::from_ref(&one)).unwrap();
    assert!(
        report.written.is_empty() && report.removed.is_empty(),
        "inbox contract: a second sync writes nothing: {report:?}"
    );

    // a new delivery adds exactly one file
    let two = mail_item(2, "two");
    let report = h.inbox().sync(&wt, &[one.clone(), two.clone()]).unwrap();
    assert_eq!(report.written, vec![two.id.clone()]);
    assert_eq!(h.inbox_files().len(), 3, "two deliveries and the index");

    // a file the worker made to look like a delivery is removed and reported
    h.plant_inbox_stray("m9.json");
    let report = h.inbox().sync(&wt, &[one.clone(), two.clone()]).unwrap();
    assert_eq!(
        report.removed.len(),
        1,
        "inbox contract: a forged file is removed and reported: {report:?}"
    );
    assert!(!h.inbox_files().contains_key("m9.json"));

    // prepare empties the outbox of an earlier attempt
    h.plant_outbox("1.json", OutboxPlant::Valid);
    h.inbox().prepare(&wt, &[one.clone(), two.clone()]).unwrap();
    assert!(
        h.outbox_names().is_empty(),
        "inbox contract: prepare empties the outbox"
    );

    // an inbox replaced by a link while the worker runs is refused, and nothing leaks through it
    h.replace_inbox_with_link();
    let err = h
        .inbox()
        .sync(&wt, &[one.clone(), two.clone(), mail_item(3, "three")])
        .expect_err("inbox contract: a link in place of the inbox is refused");
    assert!(!err.message.is_empty());
    assert!(
        h.link_target_untouched(),
        "inbox contract: nothing is written through a link"
    );
}
