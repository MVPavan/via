"""Historical unreadable-process exception (OpenCode packet §13 L14)."""
import opencode_safety as safety

ANCESTRY_ROWS=1000  # §13 L14: bounded, re-verified metadata lineage; no environment reads.
COMM_BYTES=64  # §13 L14: export a fixed class, never an arbitrary process name.


def historical_exception(proc,identity,roots,first_server_ticks,run_start_ticks,guard=None):
    """Prove strict pre-run birth and exclusion from every registered server tree (§13).

    `guard` runs before every observation and recheck and again before the
    exclusion is returned (deadline, interruption and aggregate work)."""
    guard=guard or (lambda:None)
    if type(first_server_ticks) is not int or type(run_start_ticks) is not int \
            or identity.start_ticks>=min(first_server_ticks,run_start_ticks):
        return None
    if identity in roots:return None
    guard();proc.verify(identity)
    rows=[];seen=set();pid=identity.pid
    for _ in range(ANCESTRY_ROWS):
        guard()
        if pid in seen:raise safety.Blocked('historical lock-scan ancestry cycle')
        seen.add(pid)
        row=proc.stat(pid)
        if row is None:raise safety.Blocked('historical lock-scan ancestry unavailable')
        current=safety.Identity(row['pid'],row['start_ticks'])
        if current in roots:return None
        if pid==identity.pid and current!=identity:
            raise safety.Blocked('historical lock-scan process identity changed')
        rows.append(row)
        pid=row['ppid']
        if pid<=1:break
    else:raise safety.Blocked('historical lock-scan ancestry bound')
    guard();raw=proc.read(identity,'comm',limit=COMM_BYTES).strip()
    classes={b'systemd':'systemd',b'via':'via',b'opencode':'opencode',
             b'python':'python',b'python3':'python',b'node':'node'}
    comm_class=classes.get(raw,'other');del raw
    for row in rows:
        guard()
        if proc.stat(row['pid'])!=row:
            raise safety.Blocked('historical lock-scan ancestry changed')
    guard();proc.verify(identity)
    guard()  # An exclusion is returned only inside the scan's bounds.
    return {'pid':identity.pid,'start_ticks':identity.start_ticks,'comm_class':comm_class,
            'first_server_start_ticks':first_server_ticks,'run_start_ticks':run_start_ticks,
            'disposition':'record-only'}
