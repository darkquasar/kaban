//! The writer is the store's (spec 05 SS-26, review d-4): a service assembled over a store
//! opened as an actor stamps that actor on every record and change row it writes, with no
//! second setting on the service or its write contexts to drift from it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_api::{KabanApi, RememberRequest};
use kaban_core::ActorId;
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::collections::BTreeSet;
use std::sync::Arc;

#[test]
fn the_service_stamps_the_actor_its_store_was_opened_as() {
    let s = Scratch::new("kaban-api-actor", "alice");
    let alice = ActorId::parse("alice").unwrap();
    let store = Arc::new(SqliteStore::open_as(&s.db(), &s.keys(), &alice).unwrap());
    let api = KabanApi::new(store.clone(), Policy::empty(), store.signer(), None);
    let written = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "quokka stamped through the service".into(),
            ..Default::default()
        })
        .unwrap();

    let conn = rusqlite::Connection::open(s.db()).unwrap();
    let actor: Option<String> = conn
        .query_row(
            "SELECT actor_id FROM node WHERE id = ?1",
            [&written.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actor.as_deref(), Some("alice"));
    // every change row of the write (the insert and the service's own follow-up update)
    let change_actors: BTreeSet<Option<String>> = conn
        .prepare("SELECT actor_id FROM changes")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(change_actors, BTreeSet::from([Some("alice".to_owned())]));
}
