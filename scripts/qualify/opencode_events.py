"""C1 §§5, 6.1 and 7.6 terminal evidence for OpenCode §13 qualification."""

from opencode_safety import Blocked


def terminal_event(events, session, turn, *, required=True):
    """Select one C1 turn's initial terminal and any committed late revisions."""
    if type(turn) is not int or turn < 1:
        raise Blocked('C1 event turn unavailable')
    terminal = None
    for row in events:
        if row.get('session_id') != session:
            raise Blocked('C1 event session differs')
        if row.get('turn') != turn:
            continue
        kind = row['type']
        if kind == 'turn.ended':
            if terminal is not None or row.get('late') is True:
                raise Blocked('C1 turn terminal sequence invalid')
            if row.get('state') not in {'completed', 'failed', 'cancelled', 'unknown'}:
                raise Blocked('C1 turn terminal state invalid')
            # C1 §5 counts revisions; §6.1 intentionally has no revision on turn.ended.
            terminal = {'state': row['state'], 'revision': 0}
        elif kind == 'turn.revised':
            if terminal is None or row.get('late') is not True:
                raise Blocked('C1 turn terminal sequence invalid')
            revision = row.get('revision')
            if type(revision) is not int or revision != terminal['revision'] + 1:
                raise Blocked('C1 turn revision invalid')
            if terminal['state'] != 'unknown' or row.get('from_state') != 'unknown' \
                    or row.get('state') not in {'completed', 'failed', 'cancelled'}:
                raise Blocked('C1 turn terminal state invalid')
            terminal = {'state': row['state'], 'revision': revision}
    if terminal is None and required:
        raise Blocked('C1 turn terminal event unavailable')
    return terminal
