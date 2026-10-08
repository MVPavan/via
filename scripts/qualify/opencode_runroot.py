"""Private external run roots and sanitized native diagnostics (OpenCode §13)."""
import hashlib
import functools
import json
import mmap
import math
import os
from pathlib import Path
import re
import shutil
import sqlite3
import stat
import tempfile

import opencode_safety as safety
from opencode_reply import reply_projection, VENDOR_ERROR_NAMES

# §13: source E12/E13 and strings in the hash-verified 2.0.22 executable.
DISCOVERY_NAMES = ('.claude','.agents','.claude/skills','.agents/skills','.claude/agents','.opencode',
                   'opencode.json','opencode.jsonc','AGENTS.md','CLAUDE.md','CONTEXT.md','.git')
RUN_PREFIX = 'via-oc-qual.'  # §13: runtime plus the reserved anchor tail fits 107 bytes.
SOCKET_BYTES = 107  # §13: Unix listener path bound, including the reserved anchor tail.
ANCHOR_SOCKET_TAIL = 64  # §13: maximum runtime-to-anchor socket suffix.
RUN_SUFFIX_BYTES = 8  # §13: tempfile's random suffix; actual allocation is checked too.
DISCOVERY_OFFSETS = 16  # §13: bounded fixed-name binary-string evidence per name.
CLEANUP_ENTRIES = 100000  # §13: same bounded private-tree walk as the secrecy scan.
NATIVE_ROWS = 4096  # §13: bounded diagnostic rows, never unbounded storage copies.
NATIVE_BYTES = 16 * 1024 * 1024  # §13: same observation limit as the driver.
NATIVE_PARTS = 65536  # §13: bounded content-free message/part diagnostic export.
# §13/E8/E34/E37: pinned 2.0.22 native tool names and closed diagnostic enums.
BUILTIN_TOOLS = frozenset({'shell','glob','read','grep','webfetch','websearch','write',
                          'edit','subagent','execute','patch','question','skill'})
PART_TYPES = frozenset({'text','reasoning','tool','file','agent','subtask','compaction',
                       'step-start','step-finish','patch','snapshot','retry'})
FINISH_REASONS = frozenset({'stop','length','tool-calls','content-filter','error','unknown'})
ERROR_NAMES = VENDOR_ERROR_NAMES
TOOL_STATUSES = frozenset({'pending','running','completed','error'})
MARKERS = {'local_marker':'VIA_LOCAL_AGENT_SENTINEL',
           'ancestor_agents':'VIA_ANCESTOR_AGENTS_SENTINEL',
           'ancestor_skills':'VIA_ANCESTOR_SKILL_SENTINEL'}


def create_run_root():
    """Allocate only below safe canonical XDG_RUNTIME_DIR; no /tmp fallback (§13)."""
    value=os.environ.get('XDG_RUNTIME_DIR')
    if not value: raise safety.Blocked('runtime directory unavailable')
    parent=Path(value)
    try:
        info=parent.lstat()
        valid=parent.is_absolute() and parent.resolve(strict=True)==parent \
            and stat.S_ISDIR(info.st_mode) and info.st_uid==os.getuid() \
            and stat.S_IMODE(info.st_mode)==0o700
    except (OSError,RuntimeError) as error:
        raise safety.Blocked('runtime directory unsafe') from error
    if not valid: raise safety.Blocked('runtime directory unsafe')
    pending=parent/(RUN_PREFIX+'x'*RUN_SUFFIX_BYTES)
    check_ancestors(pending,pending=True)
    if len(os.fsencode(pending/'runtime'))+ANCHOR_SOCKET_TAIL>SOCKET_BYTES:
        raise safety.Blocked('runtime socket path bound')
    lexical=Path(tempfile.mkdtemp(prefix=RUN_PREFIX,dir=parent))
    resolved=lexical.resolve(strict=True)
    if resolved!=lexical or lexical.parent!=parent:
        raise safety.Blocked('run root canonical identity differs')
    root=safety.private_directory(resolved)
    if len(os.fsencode(root/'runtime'))+ANCHOR_SOCKET_TAIL>SOCKET_BYTES:
        root.rmdir()
        raise safety.Blocked('runtime socket path bound')
    return root


def check_ancestors(root,*,pending=False):
    """Verify canonical ownership/modes and existence-only discovery names (§13)."""
    root=Path(root)
    checked=[]
    metadata=[]
    ancestors=list(root.parents)
    report={'checked_names':list(DISCOVERY_NAMES),'ancestors':checked,'clear':False,
            'directories':metadata,'basis':'pinned 2.0.22 E12/E13, binary strings; CONTEXT.md conservative'}
    def blocked(reason):
        error=safety.Blocked(reason);error.ancestor_discovery=report;return error
    try:
        if not root.is_absolute() or root.resolve(strict=not pending)!=root:
            raise blocked('run root canonical identity differs')
        if not pending:
            own=root.lstat()
            if not stat.S_ISDIR(own.st_mode) or own.st_uid!=os.getuid() \
                    or stat.S_IMODE(own.st_mode)!=0o700:
                raise blocked('run root ownership or mode unsafe')
    except (OSError,RuntimeError) as error: raise blocked('run root identity unverifiable') from error
    for index,ancestor in enumerate(ancestors):
        label='filesystem-root' if ancestor==Path('/') else (
            'runtime-parent' if index==0 else 'ancestor-'+str(index))
        checked.append(label)
        try: info=ancestor.lstat()
        except OSError as error: raise blocked('ancestor directory unverifiable') from error
        safe=stat.S_ISDIR(info.st_mode) and info.st_uid in {0,os.getuid()} and not info.st_mode&0o022
        if index==0:
            safe=safe and info.st_uid==os.getuid() and stat.S_IMODE(info.st_mode)==0o700
        metadata.append({'path_class':label,'owner_class':'invoking-user' if info.st_uid==os.getuid()
                         else 'root' if info.st_uid==0 else 'foreign',
                         'mode':format(stat.S_IMODE(info.st_mode),'04o'),'safe':bool(safe)})
        if not safe: raise blocked('ancestor directory unsafe: '+label)
        for name in DISCOVERY_NAMES:
            try: (ancestor/name).lstat()
            except FileNotFoundError: continue
            except OSError as error: raise blocked('ancestor discovery check unverifiable') from error
            raise blocked('ancestor discovery source present: '+name+' at '+label)
    report['clear']=True
    return report


def discovery_strings(binary):
    """Record fixed discovery-name offsets in the hash-verified pinned bytes (§13)."""
    proof=safety.verify_binary(binary)
    found={}
    with Path(binary).open('rb') as file,mmap.mmap(file.fileno(),0,access=mmap.ACCESS_READ) as data:
        for name in DISCOVERY_NAMES:
            needle=name.encode();offsets=[];start=0
            for _ in range(DISCOVERY_OFFSETS):
                offset=data.find(needle,start)
                if offset<0: break
                offsets.append(offset);start=offset+len(needle)
            found[name]={'literal_present':bool(offsets),'offsets':offsets,
                         'basis':'literal-string' if offsets else 'source-or-conservative'}
    if safety.sha256(binary)!=proof['sha256']:
        raise safety.Blocked('discovery binary changed during string scan')
    return {'binary_sha256':proof['sha256'],'binary_size':proof['size'],
            'names':found,'checked_names':list(DISCOVERY_NAMES),
            'context_discovery':'unverified; conservatively checked'}


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
    parent=os.environ.get('XDG_RUNTIME_DIR')
    if not parent or root.parent!=Path(parent) or not root.name.startswith(RUN_PREFIX):
        raise safety.Blocked('run root cleanup path is not owned')
    try: info=root.lstat()
    except FileNotFoundError:
        return {'removed':True,'already_absent':True,'path_class':'private-external-run-root','name':root.name}
    if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.getuid() or root.resolve(strict=True)!=root:
        raise safety.Blocked('run root cleanup ownership uncertain')
    binary=root/'bin'
    if binary.exists() or binary.is_symlink():
        info=binary.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.getuid():
            raise safety.Blocked('run program directory cleanup ownership uncertain')
        # Read-only for the run; writable only now, after process absence proof.
        binary.chmod(0o700,follow_symlinks=False)
    class RetryRemoval(Exception): pass
    def repair(_function,_path,error):
        if not isinstance(error,PermissionError): raise error
        # Restore only owned non-symlink directories, after the process proof.
        # Repair the whole tree once, then retry; never weaken a file or follow a link.
        count=0
        root.chmod(0o700,follow_symlinks=False)
        def unreadable(error):raise error  # Never claim removal after an incomplete repair walk.
        for current,directories,_files in os.walk(root,followlinks=False,onerror=unreadable):
            for name in directories:
                path=Path(current)/name;info=path.lstat();count+=1
                if count>CLEANUP_ENTRIES: raise safety.Blocked('run root cleanup entry bound')
                if stat.S_ISDIR(info.st_mode):
                    if info.st_uid!=os.getuid(): raise safety.Blocked('run root cleanup directory ownership uncertain')
                    path.chmod(0o700,follow_symlinks=False)
        raise RetryRemoval
    try: shutil.rmtree(root,onexc=repair)
    except RetryRemoval:
        try: shutil.rmtree(root)
        except OSError as error: raise safety.Blocked('private run root removal unverified') from error
    except OSError as error: raise safety.Blocked('private run root removal unverified') from error
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


def _diagnostic(function):
    @functools.wraps(function)
    def checked(*args,**kwargs):
        try: return function(*args,**kwargs)
        except (sqlite3.Error,ValueError,AttributeError,TypeError,OSError,RecursionError) as error:
            raise safety.Blocked('native diagnostic unavailable') from error
    return checked


def _message_rows(conn,protected,*,cancel_session=None):
    """Closed §13/E8 projections: no message/part text, arguments, output or paths."""
    def label(value,allowed):
        return value if type(value) is str and value in allowed \
            and not protected(value.encode()) else 'other'
    def error_name(value):
        if value is None:return None
        if type(value) is not dict:return 'other'
        return label(value.get('type',value.get('name')),ERROR_NAMES)
    def error_http(value):
        if type(value) is not dict:return {'statusCode':None,'isRetryable':None}
        details=value.get('data',value)
        if type(details) is not dict:details={}
        status=details.get('statusCode')
        retryable=details.get('isRetryable')
        return {'statusCode':status if type(status) is int and 100<=status<=599 else None,
                'isRetryable':retryable if type(retryable) is bool else None}
    def count(value):
        return value if type(value) in {int,float} and 0<=value<=2**63-1 \
            and math.isfinite(value) else None
    out=[];part_count=0
    for sid,kind,seq,raw in _rows(conn,
            'SELECT session_id,type,seq,data FROM session_message ORDER BY session_id,seq'):
        if type(sid) is not str or not re.fullmatch(r'ses_[A-Za-z0-9_]+',sid) \
                or len(sid)>256 or protected(sid.encode()):
            raise safety.Blocked('native message session identity unverifiable')
        if type(seq) is not int or not 0<=seq<=2**63-1:
            raise safety.Blocked('native message sequence unverifiable')
        if type(raw) is not str or len(raw.encode())>NATIVE_BYTES:
            raise safety.Blocked('native message diagnostic bound')
        data=json.loads(raw)
        if type(data) is not dict:raise safety.Blocked('native message shape unavailable')
        role=label(kind,{'user','assistant','system','tool'})
        content=data.get('content',[])
        if type(content) is not list:raise safety.Blocked('native message parts unavailable')
        files=data.get('files',[])
        if type(files) is not list:raise safety.Blocked('native message attachment shape unavailable')
        if part_count+len(content)+len(files)+int('text' in data)>NATIVE_PARTS:
            raise safety.Blocked('native message part diagnostic bound')
        parts=[]
        for part in content:
            if type(part) is not dict:raise safety.Blocked('native message part shape unavailable')
            item={'sessionID':sid,'type':label(part.get('type'),PART_TYPES)}
            if part.get('type')=='tool':
                state=part.get('state',{})
                if type(state) is not dict:state={}
                status=label(state.get('status'),TOOL_STATUSES|{'streaming'})
                item.update(tool_name=label(part.get('name',part.get('tool')),BUILTIN_TOOLS),
                    status='pending' if status=='streaming' else None if status=='other' else status,
                    error_name=error_name(state.get('error')),**error_http(state.get('error')))
                if sid==cancel_session and item['tool_name']=='shell':
                    arguments=state.get('input')
                    command=arguments.get('command') if type(arguments) is dict else None
                    item.update(command_equals_expected=type(command) is str
                                and command==safety.CANCEL_HELPER_COMMAND,
                                mentions_tool_helper=type(command) is str and 'tool-helper.py' in command)
            parts.append(item)
        # Native user/system messages store text and attachments outside content.
        if 'text' in data:parts.append({'sessionID':sid,'type':'text'})
        parts.extend({'sessionID':sid,'type':'file'} for _ in files)
        part_count+=len(parts)
        tokens=data.get('tokens')
        usage=None
        if type(tokens) is dict:
            cache=tokens.get('cache',{})
            if type(cache) is not dict:cache={}
            usage={**{key:count(tokens.get(key)) for key in ('input','output','reasoning')},
                   'cache_read':count(cache.get('read')),'cache_write':count(cache.get('write'))}
        out.append({'sessionID':sid,'seq':seq,'role':role,'parts':parts,
            'finish':None if data.get('finish') is None else label(data['finish'],FINISH_REASONS),
            'error_name':error_name(data.get('error')),**error_http(data.get('error')),'tokens':usage})
    return out


@_diagnostic
def vendor_message_facts(root,protected,ownership,*,cancel_session=None):
    """Snapshot every owned native session's messages before cleanup can fail (§13)."""
    root=Path(root);out=[]
    for database in sorted(root.rglob('opencode.db')):
        classified=ownership.classify(database)
        if classified is None or classified.kind!='vendor-private':
            raise safety.Blocked('native vendor database ownership unverifiable')
        _regular(database)
        with sqlite3.connect('file:'+str(database)+'?mode=ro',uri=True,timeout=1) as conn:
            out.append({'path':database.relative_to(root).as_posix(),
                'class':'native-vendor-session-storage',
                'messages':_message_rows(conn,protected,cancel_session=cancel_session)})
    return out


@_diagnostic
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


@_diagnostic
def native_facts(root,protected,ownership,*,cancel_session=None):
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
            messages=_message_rows(conn,protected,cancel_session=cancel_session)
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
    projects=[(os.fsencode(path.parent),path.parent.relative_to(root).as_posix())
              for path in sorted(root.rglob('opencode.json'))]
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
            for encoded,relative in projects:
                if encoded in line: locations.append(relative)
            rows.append({'line':number,'sha256':hashlib.sha256(line).hexdigest(),'labels':labels,
                'time':stamp[1].decode() if stamp else None,'locations':locations})
            if len(rows)>NATIVE_ROWS: raise safety.Blocked('native log diagnostic row bound')
        out.append({'path':path.relative_to(root).as_posix(),'class':classified.kind,
                    'size':info.st_size,'sha256':safety.sha256(path),'log_facts':rows})
    return out
