//! playlist4 change encoding: the bodies the desktop client sends to
//! `/playlist/v2/playlist/{id}/changes` and
//! `/playlist/v2/user/{user}/rootlist/changes` (`ListChanges`), and to
//! `POST /playlist/v2/playlist` (`ListUpdateRequest`) to create a list.
//!
//! Pure functions, unit-tested by decoding the bytes again. The endpoint
//! shapes follow the playlist4 protocol as librespot's protobufs describe it;
//! they have not been verified against the live service.

use protobuf::{Message as _, MessageField};
use spotify_proto::playlist4_external::{
    Add, CreateListReply, Delta, Item, ListAttributes, ListAttributesPartialState, ListChanges,
    ListUpdateRequest, Mov, Op, Rem, UpdateListAttributes, op::Kind,
};

use crate::error::{Error, Result};

/// One edit of a playlist's contents or name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Append tracks (`spotify:track:…` URIs).
    Add(Vec<String>),
    /// Remove entries at their current positions: `(index, uri)`.
    Remove(Vec<(u32, String)>),
    /// Move the entry at `from` so it ends up at index `to` (final position).
    Move {
        from: u32,
        to: u32,
    },
    Rename(String),
}

fn item(uri: &str) -> Item {
    let mut i = Item::new();
    i.set_uri(uri.to_owned());
    i
}

fn index(i: u32) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

/// The `Mov.to_index` that puts an entry at final position `to`: the server
/// inserts before the original item at that index, so moving down needs +1.
pub fn insert_before_index(from: u32, to: u32) -> u32 {
    if to > from { to + 1 } else { to }
}

fn op(kind: Kind) -> Op {
    let mut o = Op::new();
    o.set_kind(kind);
    o
}

/// Ops for an edit of a playlist's contents or name.
pub fn ops_for(edit: &Edit) -> Vec<Op> {
    match edit {
        Edit::Add(uris) => {
            let mut add = Add::new();
            add.items = uris.iter().map(|u| item(u)).collect();
            add.set_add_last(true);
            let mut o = op(Kind::ADD);
            o.add = MessageField::some(add);
            vec![o]
        }
        Edit::Remove(entries) => {
            // One REM per contiguous run would be tighter; one per entry,
            // highest index first, keeps earlier indices valid.
            let mut sorted: Vec<&(u32, String)> = entries.iter().collect();
            sorted.sort_by_key(|(i, _)| std::cmp::Reverse(*i));
            sorted
                .into_iter()
                .map(|(i, uri)| {
                    let mut rem = Rem::new();
                    rem.set_from_index(index(*i));
                    rem.set_length(1);
                    rem.items = vec![item(uri)];
                    let mut o = op(Kind::REM);
                    o.rem = MessageField::some(rem);
                    o
                })
                .collect()
        }
        Edit::Move { from, to } => {
            let mut mov = Mov::new();
            mov.set_from_index(index(*from));
            mov.set_length(1);
            mov.set_to_index(index(insert_before_index(*from, *to)));
            let mut o = op(Kind::MOV);
            o.mov = MessageField::some(mov);
            vec![o]
        }
        Edit::Rename(name) => vec![attributes_op(name)],
    }
}

fn attributes_op(name: &str) -> Op {
    let mut values = ListAttributes::new();
    values.set_name(name.to_owned());
    let mut state = ListAttributesPartialState::new();
    state.values = MessageField::some(values);
    let mut upd = UpdateListAttributes::new();
    upd.new_attributes = MessageField::some(state);
    let mut o = op(Kind::UPDATE_LIST_ATTRIBUTES);
    o.update_list_attributes = MessageField::some(upd);
    o
}

fn changes(base_revision: &[u8], ops: Vec<Op>) -> Vec<u8> {
    let mut delta = Delta::new();
    delta.ops = ops;
    let mut c = ListChanges::new();
    c.set_base_revision(base_revision.to_vec());
    c.deltas = vec![delta];
    c.set_want_resulting_revisions(true);
    c.set_want_sync_result(true);
    c.write_to_bytes().unwrap_or_default()
}

/// `ListChanges` body for `/playlist/v2/playlist/{id}/changes`.
pub fn playlist_changes(base_revision: &[u8], edit: &Edit) -> Vec<u8> {
    changes(base_revision, ops_for(edit))
}

/// `ListUpdateRequest` body for `POST /playlist/v2/playlist`.
pub fn create_request(name: &str) -> Vec<u8> {
    let mut attrs = ListAttributes::new();
    attrs.set_name(name.to_owned());
    let mut r = ListUpdateRequest::new();
    r.attributes = MessageField::some(attrs);
    r.write_to_bytes().unwrap_or_default()
}

/// Playlist id from a `CreateListReply` (`spotify:playlist:<id>`).
pub fn parse_created(bytes: &[u8]) -> Result<String> {
    let r = CreateListReply::parse_from_bytes(bytes)
        .map_err(|_| Error::Protocol("create playlist: bad protobuf".into()))?;
    r.uri()
        .strip_prefix("spotify:playlist:")
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(str::to_owned)
        .ok_or_else(|| Error::Protocol("create playlist: unexpected uri".into()))
}

/// Add a playlist to the top of the user's rootlist (the sidebar).
pub fn rootlist_add(base_revision: &[u8], playlist_uri: &str) -> Vec<u8> {
    let mut add = Add::new();
    add.items = vec![item(playlist_uri)];
    add.set_add_first(true);
    let mut o = op(Kind::ADD);
    o.add = MessageField::some(add);
    changes(base_revision, vec![o])
}

/// Remove a playlist from the rootlist (how the desktop client deletes or
/// unfollows a playlist).
pub fn rootlist_remove(base_revision: &[u8], playlist_uri: &str) -> Vec<u8> {
    let mut rem = Rem::new();
    rem.items = vec![item(playlist_uri)];
    rem.set_items_as_key(true);
    let mut o = op(Kind::REM);
    o.rem = MessageField::some(rem);
    changes(base_revision, vec![o])
}

/// Kind of an op, for tests.
pub fn kind_of(o: &Op) -> Option<Kind> {
    o.kind.and_then(|k| k.enum_value().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(b: &[u8]) -> ListChanges {
        ListChanges::parse_from_bytes(b).unwrap()
    }

    #[test]
    fn add_appends_items() {
        let c = decode(&playlist_changes(
            b"rev",
            &Edit::Add(vec!["spotify:track:a".into(), "spotify:track:b".into()]),
        ));
        assert_eq!(c.base_revision(), b"rev");
        let ops = &c.deltas[0].ops;
        assert_eq!(ops.len(), 1);
        assert_eq!(kind_of(&ops[0]), Some(Kind::ADD));
        let add = &ops[0].add;
        assert!(add.add_last());
        assert_eq!(add.items[1].uri(), "spotify:track:b");
    }

    #[test]
    fn removes_go_highest_index_first() {
        let c = decode(&playlist_changes(
            b"r",
            &Edit::Remove(vec![
                (1, "spotify:track:a".into()),
                (7, "spotify:track:b".into()),
            ]),
        ));
        let ops = &c.deltas[0].ops;
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].rem.from_index(), 7);
        assert_eq!(ops[0].rem.length(), 1);
        assert_eq!(ops[1].rem.from_index(), 1);
        assert_eq!(ops[1].rem.items[0].uri(), "spotify:track:a");
    }

    #[test]
    fn move_converts_final_index_to_insert_before() {
        assert_eq!(insert_before_index(2, 5), 6);
        assert_eq!(insert_before_index(5, 2), 2);
        let c = decode(&playlist_changes(b"r", &Edit::Move { from: 2, to: 5 }));
        let m = &c.deltas[0].ops[0].mov;
        assert_eq!((m.from_index(), m.length(), m.to_index()), (2, 1, 6));
    }

    #[test]
    fn rename_updates_the_name_attribute() {
        let c = decode(&playlist_changes(b"r", &Edit::Rename("Road trip".into())));
        let o = &c.deltas[0].ops[0];
        assert_eq!(kind_of(o), Some(Kind::UPDATE_LIST_ATTRIBUTES));
        assert_eq!(
            o.update_list_attributes.new_attributes.values.name(),
            "Road trip"
        );
    }

    #[test]
    fn create_and_parse_reply() {
        let r = ListUpdateRequest::parse_from_bytes(&create_request("Mix")).unwrap();
        assert_eq!(r.attributes.name(), "Mix");
        let mut reply = CreateListReply::new();
        reply.set_uri("spotify:playlist:37i9dQZF1DX".into());
        let id = parse_created(&reply.write_to_bytes().unwrap()).unwrap();
        assert_eq!(id, "37i9dQZF1DX");
        let mut bad = CreateListReply::new();
        bad.set_uri("spotify:user:x".into());
        assert!(parse_created(&bad.write_to_bytes().unwrap()).is_err());
    }

    #[test]
    fn rootlist_ops() {
        let c = decode(&rootlist_add(b"r", "spotify:playlist:p"));
        assert!(c.deltas[0].ops[0].add.add_first());
        let c = decode(&rootlist_remove(b"r", "spotify:playlist:p"));
        let rem = &c.deltas[0].ops[0].rem;
        assert!(rem.items_as_key());
        assert_eq!(rem.items[0].uri(), "spotify:playlist:p");
    }
}
