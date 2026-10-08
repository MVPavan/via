"""Private external run roots and sanitized native diagnostics (OpenCode §13)."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import stat
import tempfile

import opencode_safety as safety
from opencode_reply import reply_projection

# §13: source E12/E13 and strings in the hash-verified 2.0.22 executable.
DISCOVERY_NAMES = ('.claude','.agents','.claude/skills','.agents/skills','.claude/agents','.opencode',
                   'opencode.json','opencode.jsonc','AGENTS.md','CLAUDE.md','.git')
RUN_PREFIX = 'via-oc-qual.'  # §13: runtime plus the reserved anchor tail fits 107 bytes.
NATIVE_ROWS = 4096  # §13: bounded diagnostic rows, never unbounded storage copies.
NATIVE_BYTES = 16 * 1024 * 1024  # §13: same observation limit as the driver.
MARKERS = {'local_marker':'VIA_LOCAL_AGENT_SENTINEL',
           'ancestor_agents':'VIA_ANCESTOR_AGENTS_SENTINEL',
           'ancestor_skills':'VIA_ANCESTOR_SKILL_SENTINEL'}


def create_run_root():
    """Create a fresh owned 0700 root directly in /tmp, independent of TMPDIR (§13)."""
    return safety.private_directory(Path(tempfile.mkdtemp(prefix=RUN_PREFIX,dir='/tmp')))


def check_ancestors(root):
    """Existence-only discovery checks; never inspect ancestor file contents (§13)."""
    checked=[]
    ancestors=list(Path(root).parents)
    report={'checked_names':list(DISCOVERY_NAMES),'ancestors':checked,'clear':False,
            'basis':'pinned 2.0.22 E12/E13 and binary strings'}
    def blocked(reason):
        error=safety.Blocked(reason);error.ancestor_discovery=report;return error
    for index,ancestor in enumerate(ancestors):
        label='filesystem-root' if ancestor==Path('/') else (
            'temporary-directory' if ancestor==Path('/tmp') else 'ancestor-'+str(index))
        checked.append(label)
        for name in DISCOVERY_NAMES:
            try: (ancestor/name).lstat()
            except FileNotFoundError: continue
            except OSError as error: raise blocked('ancestor discovery check unverifiable') from error
            raise blocked('ancestor discovery source present: '+name+' at '+label)
    report['clear']=True
    return report


def copy_program(source,destination):
    """Copy hash-bound executables; no hard links or repository-visible program paths (§13)."""
    source=Path(source);destination=Path(destination)
    info=source.lstat()
    if not stat.S_ISREG(info.st_mode) or not info.st_mode&0o111:
        raise safety.Blocked('run program source is not a regular executable')
    expected=safety.sha256(source)
    safety.private_directory(destination.parent)
    with source.open('rb') as src,destination.open('xb') as dst:
        shutil.copyfileobj(src,dst,1024*1024)
    destination.chmod(0o500)
    if safety.sha256(destination)!=expected or safety.sha256(source)!=expected:
        raise safety.Blocked('run program copy hash mismatch')
    return destination


def remove_run_root(root,proof):
    """Remove only our fresh root after complete process/pgrep absence proof (§13)."""
    if any(proof.get(key) is not True for key in ('proven','processes_gone','pgrep_clear')):
        raise safety.Blocked('run root cleanup needs process absence')
    root=Path(root)
    if root.parent!=Path('/tmp') or not root.name.startswith(RUN_PREFIX):
        raise safety.Blocked('run root cleanup path is not owned')
    safety.private_directory(root)
    binary=root/'bin'
    if binary.exists() or binary.is_symlink():
        info=binary.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.getuid():
            raise safety.Blocked('run program directory cleanup ownership uncertain')
        # Read-only for the run; writable only now, after process absence proof.
        binary.chmod(0o700,follow_symlinks=False)
    shutil.rmtree(root)
    if root.exists() or root.is_symlink(): raise safety.Blocked('private run root remains')
    return {'removed':True,'path_class':'private-external-run-root','name':root.name}


def _regular(path):
    info=path.lstat()
    if not stat.S_ISREG(info.st_mode): raise safety.Blocked('native diagnostic source is non-regular')
    return info


def _rows(conn,query,args=()):
    rows=conn.execute(query,args).fetchmany(NATIVE_ROWS+1)
    if len(rows)>NATIVE_ROWS: raise safety.Blocked('native diagnostic row bound')
    return rows


def instruction_facts(database,session=None):
    """Keep instruction-state hashes and sentinel Booleans, never instruction text (§13)."""
    _regular(database);out=[]
    with sqlite3.connect('file:'+str(database)+'?mode=ro',uri=True) as conn:
        query='SELECT session_id,epoch_start,through_seq,initial_values,current_values FROM instruction_state'
        args=() if session is None else (session,)
        if session is not None: query+=' WHERE session_id=?'
        for sid,epoch,seq,initial,current in _rows(conn,query,args):
            if type(sid) is not str or not re.fullmatch(r'ses_[A-Za-z0-9_]+',sid):
                raise safety.Blocked('native instruction session identity unverifiable')
            row={'sessionID':sid,'epoch_start':epoch,'through_seq':seq,'snapshots':[]}
            for label,raw in (('initial',initial),('current',current)):
                if len(raw.encode())>NATIVE_BYTES: raise safety.Blocked('native instruction bound')
                refs=json.loads(raw)
                if type(refs) is not dict or len(refs)>64 or 'core/instructions' not in refs:
                    raise safety.Blocked('native instruction references unverifiable')
                snapshot={'label':label,'blobs':[],**{key:False for key in MARKERS}}
                for key in ('core/instructions','core/skill-guidance'):
                    digest=refs.get(key)
                    if digest is None: continue
                    if type(digest) is not str or not re.fullmatch(r'[a-f0-9]{64}',digest):
                        raise safety.Blocked('native instruction reference unverifiable')
                    blob=conn.execute('SELECT value FROM instruction_blob WHERE hash=?',(digest,)).fetchone()
                    if blob is None or type(blob[0]) is not str:
                        raise safety.Blocked('native instruction blob missing')
                    data=blob[0].encode()
                    if len(data)>NATIVE_BYTES: raise safety.Blocked('native instruction blob bound')
                    # Native keys hash the vendor representation; record an independent
                    # SHA-256 of the stored JSON bytes, without equating the two.
                    json.loads(data)
                    snapshot['blobs'].append({'key':key,'native_reference':digest,
                        'sha256':hashlib.sha256(data).hexdigest(),'size':len(data)})
                    for field,marker in MARKERS.items():
                        snapshot[field]=snapshot[field] or marker.encode() in data
                row['snapshots'].append(snapshot)
            out.append(row)
    return out


def native_facts(root,protected,ownership):
    """Read only owned native session/log/Store tables; retain closed projections (§13)."""
    root=Path(root);out=[]
    def project(value):
        return reply_projection(json.dumps(value,allow_nan=False).encode(),protected)
    for database in sorted(root.rglob('opencode.db')):
        classified=ownership.classify(database)
        if classified is None or classified.kind!='vendor-private':
            raise safety.Blocked('native vendor database ownership unverifiable')
        _regular(database)
        with sqlite3.connect('file:'+str(database)+'?mode=ro',uri=True) as conn:
            sessions=[]
            for sid,directory,model,cost,tin,tout in _rows(conn,
                    'SELECT id,directory,model,cost,tokens_input,tokens_output FROM session_v2'):
                location=Path(directory)
                if not location.is_relative_to(root): raise safety.Blocked('native session escaped run root')
                sessions.append({'location':location.relative_to(root).as_posix(),
                    'facts':project({'id':sid,'model':json.loads(model) if model else None,
                                     'cost':{'usd':cost},'tokens_input':tin,'tokens_output':tout})})
            messages=[{'type':kind if kind in {'user','assistant','idle','tool','error','compaction'} else 'other',
                       'seq':seq,'facts':reply_projection(raw.encode(),protected)}
                for kind,seq,raw in _rows(conn,'SELECT type,seq,data FROM session_message ORDER BY seq')]
        out.append({'path':database.relative_to(root).as_posix(),'class':'native-vendor-session-storage',
                    'sessions':sessions,'messages':messages,'instruction_state':instruction_facts(database)})
    for state in (entry for entry in ownership.ordered() if entry.state and entry.started):
        database=state.path/'store.sqlite3';_regular(database)
        with sqlite3.connect('file:'+str(database)+'?mode=ro',uri=True) as source,sqlite3.connect(':memory:') as conn:
            source.backup(conn)
            sessions=[project({'state':status,'model':json.loads(params).get('model'),
                'vendor_session_id':sid}) for status,params,sid in _rows(conn,
                    'SELECT state,params,vendor_session_id FROM sessions')]
            turns=[project({'state':status,'data':json.loads(raw) if raw else None})
                for status,raw in _rows(conn,'SELECT state,envelope FROM turns')]
            events=[project({'type':kind}) for (kind,) in _rows(conn,'SELECT type FROM events ORDER BY rowid')]
        out.append({'path':database.relative_to(root).as_posix(),'class':'via-store-backup',
                    'sessions':sessions,'turns':turns,'events':events})
    for path in sorted(root.rglob('*.log')):
        classified=ownership.classify(path)
        if classified is None: raise safety.Blocked('native log ownership unverifiable')
        info=_regular(path)
        if info.st_size>NATIVE_BYTES: raise safety.Blocked('native log diagnostic bound')
        rows=[]
        for number,line in enumerate(path.read_bytes().splitlines(),1):
            labels=[label for label in ('location services booted','config.updated','provider.updated',
                'shutdown','error','/api/config','/api/model','/api/session','/api/info') if label.encode() in line]
            if not labels: continue
            stamp=re.search(rb'\b(\d{4}-\d\d-\d\d[T ]\d\d:\d\d:\d\d(?:\.\d+)?)',line)
            locations=[]
            for project in sorted(root.rglob('opencode.json')):
                if os.fsencode(project.parent) in line:
                    locations.append(project.parent.relative_to(root).as_posix())
            rows.append({'line':number,'sha256':hashlib.sha256(line).hexdigest(),'labels':labels,
                'time':stamp[1].decode() if stamp else None,'locations':locations})
            if len(rows)>NATIVE_ROWS: raise safety.Blocked('native log diagnostic row bound')
        out.append({'path':path.relative_to(root).as_posix(),'class':classified.kind,
                    'size':info.st_size,'sha256':safety.sha256(path),'log_facts':rows})
    return out
