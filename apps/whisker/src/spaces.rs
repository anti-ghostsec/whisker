//! The room list as a tree: spaces are folders holding their rooms (and
//! sub-spaces); rooms in no space sit at the top level.

use matrix_sdk::ruma::OwnedRoomId;
use std::collections::HashSet;
use whisker_core::timeline::RoomSummary;

/// One line of the room list.
#[derive(Debug, Clone)]
pub struct Line<'a> {
    pub room: &'a RoomSummary,
    pub depth: usize,
    /// For spaces: whether its rooms are shown.
    pub open: bool,
    /// For spaces: unread messages in the rooms inside (all levels).
    pub unread: u64,
    /// For spaces: how many of its rooms you're in.
    pub rooms: usize,
}

/// `rooms` (already in list order) as folder lines. Spaces in `closed` show
/// without their rooms. With a search, it's a flat list of matching rooms.
pub fn tree<'a>(rooms: &'a [RoomSummary], closed: &HashSet<OwnedRoomId>, search: &str) -> Vec<Line<'a>> {
    let line = |room, depth| Line { room, depth, open: false, unread: 0, rooms: 0 };
    if !search.is_empty() {
        return rooms.iter().filter(|r| !r.is_space && r.name.to_lowercase().contains(search)).map(|r| line(r, 0)).collect();
    }
    let joined: HashSet<&OwnedRoomId> = rooms.iter().map(|r| &r.room_id).collect();
    // Rooms listed by a space we're in don't also show at the top.
    let listed: HashSet<&OwnedRoomId> = rooms.iter().filter(|r| r.is_space).flat_map(|s| s.children.iter()).filter(|c| joined.contains(c)).collect();
    let mut out = Vec::new();
    // Invitations first, then spaces, then the rest, each in list order.
    let top = |r: &&RoomSummary| !listed.contains(&r.room_id) || r.invite;
    for r in rooms.iter().filter(top).filter(|r| r.invite) {
        out.push(line(r, 0));
    }
    let mut reached: HashSet<&OwnedRoomId> = HashSet::new();
    for s in rooms.iter().filter(top).filter(|r| r.is_space && !r.invite) {
        add_space(s, 0, rooms, closed, &mut vec![&s.room_id], &mut reached, &mut out);
    }
    // Spaces that only list each other (a sub-space listing its parent)
    // aren't under any top-level one: show them at the top too.
    for s in rooms.iter().filter(|r| r.is_space && !r.invite) {
        if !reached.contains(&s.room_id) {
            add_space(s, 0, rooms, closed, &mut vec![&s.room_id], &mut reached, &mut out);
        }
    }
    for r in rooms.iter().filter(top).filter(|r| !r.is_space && !r.invite) {
        out.push(line(r, 0));
    }
    out
}

fn add_space<'a>(
    space: &'a RoomSummary,
    depth: usize,
    rooms: &'a [RoomSummary],
    closed: &HashSet<OwnedRoomId>,
    path: &mut Vec<&'a OwnedRoomId>,
    reached: &mut HashSet<&'a OwnedRoomId>,
    out: &mut Vec<Line<'a>>,
) {
    reached.insert(&space.room_id);
    let at = out.len();
    let open = !closed.contains(&space.room_id);
    out.push(Line { room: space, depth, open, unread: 0, rooms: 0 });
    // Children in the room list's order (most recent first), spaces first.
    let kids: HashSet<&OwnedRoomId> = space.children.iter().collect();
    let mut kid_rooms: Vec<&RoomSummary> = rooms.iter().filter(|r| kids.contains(&r.room_id) && !r.invite).collect();
    kid_rooms.sort_by_key(|r| !r.is_space);
    let (mut unread, mut count) = (0, 0);
    for k in kid_rooms {
        // A space listing itself, or its parent: don't loop.
        if path.contains(&&k.room_id) {
            continue;
        }
        if k.is_space {
            path.push(&k.room_id);
            let mut inner = Vec::new();
            add_space(k, depth + 1, rooms, closed, path, reached, &mut inner);
            path.pop();
            unread += inner[0].unread;
            count += inner[0].rooms;
            if open {
                out.extend(inner);
            }
        } else {
            unread += k.unread;
            count += 1;
            if open {
                out.push(Line { room: k, depth: depth + 1, open: false, unread: 0, rooms: 0 });
            }
        }
    }
    out[at].unread = unread;
    out[at].rooms = count;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(id: &str, space: bool, kids: &[&str], unread: u64) -> RoomSummary {
        RoomSummary {
            room_id: OwnedRoomId::try_from(format!("!{id}:x")).unwrap(),
            name: id.into(),
            invite: false,
            avatar: None,
            topic: None,
            encrypted: false,
            unread,
            members: 0,
            latest: None,
            pinned: false,
            is_space: space,
            children: kids.iter().map(|k| OwnedRoomId::try_from(format!("!{k}:x")).unwrap()).collect(),
        }
    }

    fn names(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| format!("{}{}", "  ".repeat(l.depth), l.room.name)).collect()
    }

    #[test]
    fn spaces_are_folders() {
        let rooms = vec![
            room("loose", false, &[], 1),
            room("cats", false, &[], 2),
            room("dogs", false, &[], 3),
            room("pets", true, &["cats", "inner", "notjoined"], 0),
            room("inner", true, &["dogs", "pets"], 0),
        ];
        let t = tree(&rooms, &HashSet::new(), "");
        assert_eq!(names(&t), ["pets", "  inner", "    dogs", "  cats", "loose"]);
        assert_eq!((t[0].unread, t[0].rooms), (5, 2));
        assert!(t[0].open);

        let closed = HashSet::from([OwnedRoomId::try_from("!inner:x").unwrap()]);
        let t = tree(&rooms, &closed, "");
        assert_eq!(names(&t), ["pets", "  inner", "  cats", "loose"]);
        assert_eq!(t[1].unread, 3);

        // Searching lists matching rooms, not folders.
        assert_eq!(names(&tree(&rooms, &HashSet::new(), "dog")), ["dogs"]);
    }
}
