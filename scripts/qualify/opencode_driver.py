#!/usr/bin/env python3
"""Private VIA/OpenCode transport and supervision; vendors/opencode.md §§2–4, 13.

No command runs on import. This driver is used only by the runner's explicit live
admission, or injected fake transports in self-tests. Secrets and bearer handles
remain in memory; raw vendor responses never become evidence files.
"""

import base64
import datetime
import contextlib
import errno
import fcntl
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import secrets
import selectors
import shutil
import signal
import socket
import sqlite3
import stat
import subprocess
import tempfile
import threading
import time
import urllib.parse

import opencode_safety as safety

Blocked = safety.Blocked
# Packet §9 and the reviewed qualification plan's observation/pagination bounds.
OBSERVATION_BYTES = 16 * 1024 * 1024
EVIDENCE_BYTES = 256 * 1024 * 1024
PAGE_COUNT = 1000
SOCKET_BYTES = 107
SSE_HANDSHAKE_SECONDS = 30  # §13 owned observer absolute header deadline.
WAIT_REPLY_MARGIN_SECONDS = 10  # §13: allow the CLI to return its bounded wait reply.
HELPER_STOP_SECONDS = 30  # §13 L14 bounded cooperative cleanup before a successor.
ANCHOR_SOCKET_TAIL = 64


def _typed(value, schema, where):
    if isinstance(schema, dict):
        if type(value) is not dict:
            raise Blocked(where + ': object missing')
        for name, inner in schema.items():
            if name not in value:
                raise Blocked(where + ': required field missing')
            _typed(value[name], inner, where + '.' + name)
    elif isinstance(schema, list):
        if type(value) is not list:
            raise Blocked(where + ': list missing')
        for item in value:
            _typed(item, schema[0], where)
    elif schema is not object and type(value) not in ((schema,) if isinstance(schema,type) else schema):
        raise Blocked(where + ': wrong field type')
    return value


S, I, B, N = (str,), (int,), (bool,), (type(None),)
SCHEMAS = {
    'daemon status': {'pid': I},
    'receipt': {'turn': S, 'effective': {'effort': S+N, 'max_steps': I+N}},
    'spawn receipt': {'session_id': S, 'turn': S, 'handle': S,
                      'effective': {'effort': S+N, 'max_steps': I+N}},
    'envelope': {'session_id': S, 'turn': I, 'state': S, 'failure': (dict,type(None)),
                 'stop_reason': S+N, 'vendor_version': S+N, 'vendor_session_id': S+N,
                 'final_text': S+N, 'structured_output': object, 'steps': I+N,
                 'cancel': (dict,type(None)),
                 'cost': {'usd': (int,float,type(None)), 'scope': S},
                 'usage': {'scope': S,'input_tokens': I+N,'cached_input_tokens': I+N,
                           'output_tokens': I+N},
                 'warnings': [{'code': S}],
                 'denied_actions': [{'kind': S,'target': S,'event_seq': I}],
                 'denied_actions_total': I, 'auto_declined_requests_total': I},
    'cancel reply': {'turn':S,'state': S, 'already_terminal':B,'cancel': (dict,type(None))},
    'events page': {'events': [{'seq': I,'type': S}], 'more': B,'next_after': I},
    'status': {'inherit': {key:S for key in ('agents','hooks','instruction_files',
                                          'mcp_servers','plugins','skills')},
               'warnings': [{'code':S}], 'vendor_identity_verified': B,
               'progress': (dict,type(None)), 'turns': [{'state':S}]},
    'logs': {'transcript': S+N},
    'error': {'error': {'code': I,'message': S,'data':{'kind':S}}},
    'daemon stop': {'stopping':B},
    'describe': {'harness':S,'capabilities':{'params':{'max_steps':{'support':S}}}},
    'models': {'models':list},
}


def strict_reply(raw, schema):
    """Decode exactly one JSON object, then check every field a verdict reads (C1)."""
    try:
        if len(raw) > OBSERVATION_BYTES:
            raise Blocked('CLI response exceeds observation bound')
        value=json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
    except (ValueError,UnicodeError,TypeError) as error:
        raise Blocked('CLI reply is not one complete JSON document') from error
    if schema not in SCHEMAS:
        if schema!='unknown': raise Blocked('CLI schema not registered')
        if type(value) is not dict: raise Blocked('CLI reply object missing')
        return value
    _typed(value,SCHEMAS[schema],schema)
    if schema=='cancel reply' and value['state'] not in {'running','completed','failed','cancelled','unknown'}:
        raise Blocked('cancel state invalid')
    if schema=='envelope':
        if value['state'] not in {'completed','failed','cancelled','unknown'}:
            raise Blocked('envelope terminal state invalid')
        if value['failure'] is not None:
            _typed(value['failure'],{'class':S,'message':S},'failure')
        if value['cancel'] is not None:
            _typed(value['cancel'],{'outcome':S,'cleanup':S},'cancel')
        if value['cost']['scope'] not in {'turn','vendor_interval','session_cumulative','unavailable'}:
            raise Blocked('cost scope invalid')
        if value['usage']['scope'] not in {'turn','vendor_interval','session_cumulative','unavailable'}:
            raise Blocked('usage scope invalid')
        if any(type(value['usage'][key]) is int and value['usage'][key]<0
               for key in ('input_tokens','cached_input_tokens','output_tokens')):
            raise Blocked('negative usage')
    return value


def read_pages(fetch, *, deadline=None):
    """Read bounded advancing C1 event pages; missing cursors never mean completion."""
    deadline=deadline or time.monotonic()+30
    after, result=0, []
    for _ in range(PAGE_COUNT):
        if time.monotonic()>=deadline:
            raise Blocked('event pagination deadline')
        page=fetch(after)
        _typed(page,SCHEMAS['events page'],'events page')
        for event in page['events']:
            if event['seq']<=after:
                raise Blocked('event sequence did not advance')
            after=event['seq']; result.append(event)
        cursor=page['next_after']
        if cursor<after or (page['more'] and cursor<=after and not page['events']):
            raise Blocked('event cursor did not advance')
        if not page['more']:
            return result
        if cursor<=0:
            raise Blocked('invalid event cursor')
        after=cursor
    raise Blocked('event pagination bound')


def bounded_command(argv, *, env, cwd=None, input=None, timeout=30):
    """Collect owned CLI pipes only in memory with an absolute time/byte bound."""
    try:
        process=subprocess.Popen(argv,env=env,cwd=cwd,stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    except OSError as error:
        raise Blocked('private command could not start') from error
    selector=selectors.DefaultSelector()
    output={}
    deadline=time.monotonic()+timeout
    try:
        pending=memoryview(input or b''); written=0
        if pending:
            os.set_blocking(process.stdin.fileno(),False)
            selector.register(process.stdin,selectors.EVENT_WRITE,'input')
        else: process.stdin.close()
        for label,stream in (('out',process.stdout),('err',process.stderr)):
            os.set_blocking(stream.fileno(),False)
            selector.register(stream,selectors.EVENT_READ,label); output[label]=bytearray()
        while selector.get_map():
            remaining=deadline-time.monotonic()
            if remaining<=0:
                raise Blocked('private command deadline')
            for key,_ in selector.select(min(remaining,0.1)):
                if key.data=='input':
                    try: written+=os.write(key.fileobj.fileno(),pending[written:written+65536])
                    except BrokenPipeError: written=len(pending)
                    if written==len(pending):
                        selector.unregister(key.fileobj); key.fileobj.close()
                    continue
                chunk=os.read(key.fileobj.fileno(),65536)
                if not chunk:
                    selector.unregister(key.fileobj); continue
                output[key.data].extend(chunk)
                if sum(map(len,output.values()))>OBSERVATION_BYTES:
                    raise Blocked('private command observation bound')
        return process.wait(timeout=max(0.01,deadline-time.monotonic())),bytes(output['out']),bytes(output['err'])
    except BaseException:
        # This process is the CLI we started, never a vendor/daemon discovered by name.
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)
        raise
    finally:
        selector.close()
        for stream in (process.stdin,process.stdout,process.stderr): stream.close()


def input_id(session,turn):
    """Exact §7.1 deterministic VIA input ID, matching adapters/opencode/execution.rs."""
    if type(session) is not str or type(turn) is not int or not 0<turn<2**32:
        raise Blocked('invalid seam input identity')
    digest=hashlib.sha256()
    for field in (b'via-opencode-input-v1',session.encode(),turn.to_bytes(4,'big')):
        digest.update(len(field).to_bytes(8,'little')); digest.update(field)
    value=int.from_bytes(digest.digest()[:16],'big'); digits=[]
    alphabet='0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz'
    for _ in range(22): digits.append(alphabet[value%62]); value//=62
    return 'msg_via'+''.join(reversed(digits))


class Driver:
    """Owned private driver for the reviewed phases; public secrets never persisted."""

    def __init__(self,release=None,failpoints=None,pinned=None,evidence=None,*,
                 via_release=None,via_failpoints=None,opencode=None,signals=None,
                 proc=None,execute=None,initialize=False,public_free=False,mock_origins=(),rg=None):
        release=release if release is not None else via_release
        failpoints=failpoints if failpoints is not None else via_failpoints
        pinned=pinned if pinned is not None else opencode
        if None in (release,failpoints,pinned,evidence):
            raise Blocked('explicit VIA builds, pinned program and evidence required')
        self.signals=signals
        self.paths={'release':Path(release).resolve(),'failpoints':Path(failpoints).resolve()}
        self.pinned=Path(pinned).resolve()
        self.evidence=Path(evidence).resolve()
        self.evidence.mkdir(parents=True,mode=0o700,exist_ok=True)
        if stat.S_IMODE(self.evidence.stat().st_mode)!=0o700:
            raise Blocked('evidence root is not private')
        self.proc=proc or safety.ProcReader()
        self.execute=execute or bounded_command
        self.vault=safety.PasswordVault()
        self.guard=safety.SpendingGuard(public_free=public_free,owned_mock_origins=mock_origins)
        self.mock_origins=set(mock_origins)
        self.rg=rg
        self.public_free=public_free
        self._http=None; self._binary=None; self.daemon=None; self.anchor=None; self.vendor_identity=None
        self.identities=set(); self.uncertain=[]; self.build_hashes={}; self.handles={}
        self.bearer_forms=set()
        self.events=[]; self.events_error=None; self._event_stop=threading.Event()
        self._event_thread=None; self._event_conn=None; self._events_lock=threading.Lock()
        self.counter=0; self.owned_replies=[]; self.secret_forms=[]; self._armed={}
        self.project=None; self.fixtures={}; self.provider_endpoints={}; self.catalog_cache={}
        self.automatic_models_proven=False
        self.phase_deadline=None; self.phase_kind=None; self.stop_result=None
        self.state=self.evidence/'state'; self.runtime=self.evidence/'runtime'
        self.helpers=self.evidence/'helpers'; self.home=self.evidence/'home'
        self.env={}; self.namespace=None; self.namespace_env={}; self.inventory=None
        self.journal=safety.StopJournal(self.evidence.parent/'stopped-daemon.json',self.proc)
        self.mock_providers={}; self.last_model=None; self.last_request=None; self.last_cancel=None
        self._event_generation=None; self._sigkill_at=None; self._last_rotation=None
        self._event_socket=None; self._event_watchdog=None
        self._idle_retired=False; self._last_lifecycle=None; self.inherit_settings={}
        self.phase_public_left=None; self.phase_mock_left=None; self.helper_ledger={}
        self._phase_public_used=0
        self.helper_channels={}; self.helper_generations={}; self.lifecycle_counter=0
        self.helper_origins={}; self.server_identities=set()
        self.fake_gate_manifest=None
        self._armed_history=set()
        self._seam_targets={}
        self._publication_entered=threading.Event(); self._publication_release=threading.Event()
        if initialize:
            self.prepare()

    def _record(self,label,value):
        raw=json.dumps(value,allow_nan=False).encode()
        if len(raw)>OBSERVATION_BYTES or any(form and form in raw for form in self.secret_forms):
            raise Blocked('evidence secrecy or size control failed')
        used=sum(path.stat().st_size for path in self.evidence.glob('*.json'))
        if used+len(raw)>EVIDENCE_BYTES:
            raise Blocked('run evidence bound')
        self.counter+=1
        self.vault.safe_write_json(self.evidence/f'{self.counter:05d}-{label}.json',value)

    def prepare(self):
        """Create exact namespace/private roots and reviewed minimal PATH (N1–N3,N8,N9)."""
        for directory in (self.home,self.state,self.helpers): safety.private_directory(directory)
        self.namespace,self.namespace_env=safety.create_namespace(self.state)
        safety.init_fixture_repo(self.namespace,self.home)
        path,rg_proof=safety.build_minimal_path(self.helpers,self.rg)
        if len(os.fsencode(self.runtime))+ANCHOR_SOCKET_TAIL>SOCKET_BYTES:
            # Coordinator ruling N9 explicitly authorizes short /tmp/via-* runtime roots.
            self.runtime=Path(tempfile.mkdtemp(prefix='via-ocl.',dir='/tmp'))
            self._record('runtime',{'short_root':True,'authority':'coordinator N9 socket ruling'})
        else: safety.private_directory(self.runtime)
        self.env={key:str(safety.private_directory(self.evidence/('daemon-'+part)))
                  for key,part in safety.PRIVATE_PARTS.items()}
        self.env.update(PATH=path,LANG='C.UTF-8',VIA_STATE_DIR=str(self.state),
                        VIA_RUNTIME_DIR=str(self.runtime))
        self._record('rg',rg_proof)
        # Inventory excludes helper rg's reviewed symlink; it covers Bun's HOME cache too.
        self.inventory=safety.Inventory([self.namespace,self.home,
                                        *(Path(value) for value in self.env.values()
                                          if value.startswith(str(self.evidence/'daemon-')))])
        self.recover_stopped()

    def recover_stopped(self):
        """Recover the parent-root persisted SIGSTOP identity after verified PID/ticks (N6)."""
        recovered=self.journal.recover()
        if self.journal.recovery_fact is not None:
            self._record('recovery',self.journal.recovery_fact)
        return recovered

    def build(self,kind):
        """Hash selected prebuilt VIA binaries; never builds or acquires implicitly (N4)."""
        if kind not in self.paths:
            raise Blocked('unknown VIA build selection')
        path=self.paths[kind]
        try:
            row=path.stat()
            if not stat.S_ISREG(row.st_mode) or not row.st_mode&0o111:
                raise Blocked('VIA binary is not executable')
            digest=safety.sha256(path)
        except OSError as error:
            raise Blocked('requested VIA build is unavailable') from error
        if self.fake_gate_manifest is not None:
            manifest=self.fake_gate_manifest
            _typed(manifest,{'verified':B,'via_builds':{'release':S,'failpoints':S}},'VIA build gate binding')
            if manifest['verified'] is not True or manifest['via_builds'][kind]!=digest:
                raise Blocked('VIA build changed after fake-first gate verification')
        self.build_hashes[kind]=digest
        self._record('build',{'kind':kind,'sha256':digest,'size':row.st_size})
        return digest

    def fixture(self,name,config):
        """Initialize and commit a private fixture Git boundary, never the worktree Git."""
        if not name or not name.replace('-','').replace('_','').isalnum() or name in self.fixtures:
            raise Blocked('unsafe or duplicate fixture name')
        root=safety.private_directory(self.evidence/'fixtures')
        project=root/name
        safety.init_fixture_repo(project,self.home)
        opencode=safety.private_directory(project/'.opencode')
        config=dict(config)
        if name=='long-run': self._long_run_started=time.monotonic()
        config=self._materialize_fixture(name,project,config)
        self._validate_provider_config(config)
        fd=os.open(project/'opencode.json',os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
        with os.fdopen(fd,'w') as file: json.dump(config,file,allow_nan=False)
        self.fixtures[name]={'path':project,'config':config}
        self._refresh_inventory([opencode])
        return project

    def _refresh_inventory(self,extra=()):
        roots=[self.namespace,self.home,*[Path(value) for key,value in self.env.items()
                                         if key in safety.PRIVATE_PARTS],*extra]
        roots += [row['path']/'.opencode' for row in self.fixtures.values()]
        if self.inventory: self.inventory.check()
        self.inventory=safety.Inventory(roots)

    def _validate_provider_config(self,config):
        providers=config.get('providers',{})
        if type(providers) is not dict: raise Blocked('project provider shape invalid')
        endpoints={}
        for name,row in providers.items():
            if type(row) is not dict or type(row.get('settings')) is not dict:
                raise Blocked('overridden provider needs explicit loopback endpoint')
            endpoint=row['settings'].get('baseURL')
            if not isinstance(endpoint,str) or safety.loopback_origin(endpoint) not in self.mock_origins:
                raise Blocked('overridden provider does not use owned loopback mock')
            endpoints[name]=endpoint
        return endpoints

    def start(self,kind,project):
        """Start only explicit private VIA config and verify both locks/PID identity."""
        project=Path(project)
        if self.daemon is not None and self.phase_kind==kind:
            self.proc.verify(self.daemon)
            if project not in [row['path'] for row in self.fixtures.values()] and project!=self.namespace:
                raise Blocked('project is outside initialized private fixtures')
            self.project=project
            item=next((row for row in self.fixtures.values() if row['path']==project),None)
            self.provider_endpoints={} if item is None else self._validate_provider_config(item['config'])
            return
        if self.daemon is not None: self.stop()
        if project not in [item['path'] for item in self.fixtures.values()] and project!=self.namespace:
            raise Blocked('project is outside initialized private fixtures')
        self.build('release'); self.build('failpoints')
        safety.verify_binary(self.pinned)
        safety.validate_path(self.env['PATH'],self.helpers)
        self.project=Path(project)
        config={'harnesses':{'opencode':{'binary':str(self.pinned),'inherit':self.inherit_settings}}}
        safety.verify_daemon_program(config,self.pinned)
        fd=os.open(self.state/'daemon.json',os.O_WRONLY|os.O_CREAT|os.O_TRUNC|os.O_NOFOLLOW,0o600)
        with os.fdopen(fd,'w') as file: json.dump(config,file)
        self.phase_kind=kind; self._binary=self.paths[kind]
        env=dict(self.env)
        env.pop('VIA_FAILPOINT_DIR',None); env.pop('VIA_FAILPOINT_TOKEN',None)
        if kind=='failpoints':
            directory=safety.private_directory(self.evidence/('failpoints-'+secrets.token_hex(8)))
            self._token=secrets.token_hex(24)
            env.update(VIA_FAILPOINT_DIR=str(directory),VIA_FAILPOINT_TOKEN=self._token)
        elif kind!='release': raise Blocked('invalid VIA build selection')
        self.env=env
        self.provider_endpoints={}
        for row in self.fixtures.values():
            if row['path']==self.project: self.provider_endpoints=self._validate_provider_config(row['config'])
        self._refresh_inventory()
        try:
            reply=self.via(['daemon','status'])
            row=self.proc.stat(reply['pid'])
            if row is None: raise Blocked('private daemon PID disappeared')
            identity=safety.Identity(reply['pid'],row['start_ticks'])
            if not self._locks_owned(identity.pid):
                raise Blocked('private daemon lock ownership unavailable')
            self.daemon=identity; self.identities.add(identity)
            self._record('daemon-start',{'verified':True,**identity.report(),'build':kind})
        except BaseException:
            self.uncertain.append('failed daemon start')
            self._discover_private_lockers()
            raise

    def _lock_paths(self): return (self.runtime/'daemon.lock',self.state/'store.lock')

    def _lock_rows(self):
        try: raw=(self.proc.root/'locks').read_bytes()
        except OSError as error: raise Blocked('private lock evidence unreadable') from error
        if len(raw)>OBSERVATION_BYTES: raise Blocked('lock evidence exceeds bound')
        ids={}
        for path in self._lock_paths():
            try: row=path.stat()
            except OSError as error: raise Blocked('private lock file absent') from error
            ids[f'{os.major(row.st_dev):02x}:{os.minor(row.st_dev):02x}:{row.st_ino}']=path
        result={path:[] for path in self._lock_paths()}
        for line in raw.decode('ascii','strict').splitlines():
            fields=line.replace('-> ','').split()
            if len(fields)>=6 and fields[5] in ids:
                result[ids[fields[5]]].append((int(fields[4]),fields[1]))
        return result

    def _locks_owned(self,pid):
        return all((pid,'FLOCK') in rows for rows in self._lock_rows().values())

    def _discover_private_lockers(self):
        try:
            for rows in self._lock_rows().values():
                for pid,_ in rows:
                    row=self.proc.stat(pid)
                    if row is None: self.uncertain.append('unreadable private locker')
                    else: self.identities.add(safety.Identity(pid,row['start_ticks']))
        except Blocked: self.uncertain.append('private lockers unverifiable')

    def via(self,args):
        """Call selected private CLI; bearer handles use stdin and replies stay in memory."""
        if not self._binary: raise Blocked('VIA build not selected')
        args=list(args)
        if not args: raise Blocked('CLI verb missing')
        verb=args[0]
        if verb in {'spawn','resume','steer'} and '--max-steps' not in args:
            self.spending_check(args=args)
            self._admit_model(self.last_model)
        if '--json' not in args: args.append('--json')
        cleanup=verb=='daemon' and len(args)>1 and args[1]=='stop'
        if self.phase_deadline and not cleanup and time.monotonic()>=self.phase_deadline:
            raise Blocked('phase deadline')
        if len(args)>1 and args[1] in self.handles and any(flag in args for flag in ('--handle','--handle-file')):
            raise Blocked('bearer handle must remain in memory')
        input=None; prompt_path=None
        prompt=None
        if '--prompt' in args:
            index=args.index('--prompt')
            prompt=args[index+1]
            if len(prompt.encode())>32768:
                if len(args)>1 and args[1] in self.handles:
                    raw=prompt.encode()
                    if self.vault.leaks(raw) or any(form and form in raw for form in self.secret_forms):
                        raise Blocked('private prompt file would contain a protected secret')
                    prompt_path=self.evidence/('prompt-'+secrets.token_hex(8)+'.tmp')
                    fd=os.open(prompt_path,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
                    with os.fdopen(fd,'wb') as output: output.write(raw)
                    args[index:index+2]=['--prompt-file',str(prompt_path)]
                else:
                    args[index:index+2]=['--prompt-file','-']; input=prompt.encode()
        if len(args)>1 and args[1] in self.handles and verb in {'resume','cancel','close','steer'}:
            if any(flag in args for flag in ('--handle','--handle-file')):
                raise Blocked('bearer handle must remain in memory')
            args.append('--handle-stdin'); input=(self.handles[args[1]]+'\n').encode()+(input or b'')
        timeout=30
        if verb=='wait':
            cli_timeout=180000
            if '--timeout-ms' in args:
                try: cli_timeout=int(args[args.index('--timeout-ms')+1])
                except (ValueError,IndexError) as error: raise Blocked('invalid CLI wait timeout') from error
                if not 0<=cli_timeout<=180000: raise Blocked('CLI wait timeout exceeds phase ceiling')
            timeout=cli_timeout/1000+WAIT_REPLY_MARGIN_SECONDS
        if self.phase_deadline and not cleanup: timeout=min(timeout,max(0.01,self.phase_deadline-time.monotonic()))
        try:
            command_started=time.monotonic()
            rc,out,err=self.execute([str(self._binary),*args],env=self.env,cwd=self.project,
                                    input=input,timeout=timeout)
        finally:
            if prompt_path is not None: prompt_path.unlink(missing_ok=True)
        if self.vault.leaks(out) or self.vault.leaks(err) or any(form in out+err for form in self.secret_forms):
            raise Blocked('secret found in CLI output; output not persisted')
        if rc==4: raise Blocked('private daemon unreachable')
        if rc not in {0,3}:
            value=strict_reply(err or out,'error')
            if 'error' not in value and 'code' not in value:
                raise Blocked('CLI error schema unverifiable')
            self._record('cli-error',{'verb':verb,'rc':rc,'typed':True})
            return {'cli_error':value,'exit_code':rc}
        schema={'status':'status','wait':'envelope','events':'events page','logs':'logs',
                'cancel':'cancel reply'}.get(verb,'unknown')
        if verb=='daemon' and len(args)>1:
            schema='daemon status' if args[1]=='status' else 'daemon stop'
        if verb in {'models','describe'}: schema=verb
        if verb=='resume': schema='receipt'
        if verb=='spawn': schema='spawn receipt' if '--background' in args else 'envelope'
        value=strict_reply(out,schema)
        if 'handle' in value:
            if type(value.get('session_id')) is not str or type(value['handle']) is not str:
                raise Blocked('bearer receipt malformed')
            self.handles[value['session_id']]=value['handle']
            self.secret_forms.append(value['handle'].encode())
            self.bearer_forms.add(value['handle'].encode())
        if schema=='envelope': self.account(value)
        self.owned_replies.append(value)
        if verb in {'spawn','resume'}:
            internal=dict(value)
            if verb=='resume': internal['session_id']=args[1]
            self.last_request={'verb':verb,'receipt':internal,'prompt':prompt,'args':args}
        if verb=='cancel': self.last_cancel={'at':time.monotonic(),'reply':value,'held':dict(self._armed),
                                            'elapsed_ms':int((time.monotonic()-command_started)*1000)}
        self._record('cli',{'verb':verb,'rc':rc,'schema':schema,'fields':sorted(value),
                            'handle_persisted':False,'stderr_present':bool(err)})
        if self.inventory: self.inventory.check()
        return value

    def _host_record(self):
        path=self.state/'store.sqlite3'
        try:
            with sqlite3.connect(f'file:{path}?mode=ro',uri=True,timeout=1) as connection:
                rows=connection.execute('SELECT generation,pid,start_ticks,vendor_pid,phase,owner_server '
                                        'FROM anchors WHERE owner_server IS NOT NULL '
                                        'AND absence_time IS NULL').fetchall()
        except (sqlite3.Error,OSError) as error:
            raise Blocked('owned Host launch record unavailable') from error
        rows=[row for row in rows if row[1] and row[2] is not None and row[3]]
        if len(rows)!=1: raise Blocked('owned Host generation ambiguous')
        generation,pid,ticks,vendor_pid,phase,server_id=rows[0]
        return {'generation':generation,'anchor':safety.Identity(pid,ticks),
                'vendor_pid':vendor_pid,'phase':phase,'server_id':server_id}

    def ensure_vendor(self):
        """Acquire through VIA models, then authenticate exactly the Host-recorded child."""
        if not self.daemon: raise Blocked('owned daemon required')
        if self._http and self.vendor_identity and self.proc.alive(self.vendor_identity) is True:
            return
        self.via(['models','--harness','opencode'])
        record=self._host_record(); self.proc.verify(record['anchor'])
        row=self.proc.stat(record['vendor_pid'])
        if row is None or row['ppid']!=record['anchor'].pid:
            raise Blocked('vendor is not Host anchor child')
        identity=safety.Identity(row['pid'],row['start_ticks']); self.proc.verify(identity)
        previous=self.vendor_identity
        password=self.vault.read_once(self.proc,identity)
        origin=self.proc.listener(identity)
        self._http=safety.OwnedHTTP(origin,identity,self.proc,password)
        status,raw=self._http.request('GET','/api/info')
        value=self._json(raw)
        _typed(value,{'pid':I,'version':S},'vendor info')
        if status!=200 or value['pid']!=identity.pid or value['version']!='2.0.22':
            raise Blocked('owned vendor handshake mismatch')
        self.vendor_identity=identity; self.anchor=record['anchor']
        self.server_identities.add(identity)
        self.identities.update((identity,self.anchor))
        rotated=None if previous is None else self.vault.changed(previous,identity)
        self._last_rotation=rotated
        self._record('vendor-auth',{'owned_pid':identity.pid,'version':value['version'],
                                    'password_changed':rotated,'password_written':False})
        if self._event_generation!=identity:
            self.start_event_capture(); self._event_generation=identity

    @staticmethod
    def _json(raw):
        try: return json.loads(raw,parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
        except (ValueError,UnicodeError) as error:
            raise Blocked('vendor JSON shape unavailable') from error

    def vendor(self,method,path,body=None):
        """Memory-only authenticated API to one positively verified owned generation."""
        if urllib.parse.urlsplit(path).path=='/api/credential':
            raise Blocked('credential read/write requires isolated direct seeding')
        self.ensure_vendor()
        if method.upper() in {'POST','PUT','PATCH'} and any(
                word in urllib.parse.urlsplit(path).path for word in ('/prompt','/compact','/fork')):
            self.spending_check(body=body,path=path)
            self._admit_model(self.last_model)
        status,raw=self._http.request(method,path,body)
        if self.vault.leaks(raw): raise Blocked('password in vendor response')
        value=None if not raw else self._json(raw)
        self._record('vendor-request',{'method':method,'status':status,
                                      'route':self._route_name(path),'bytes':len(raw)})
        return {'status':status,'body':value}

    @staticmethod
    def _route_name(path):
        parts=urllib.parse.urlsplit(path).path.split('/')
        return '/'.join('<id>' if part.startswith(('ses_','msg_','in_')) else part for part in parts)

    def transcript(self,vendorsid):
        """Read only the owned session's bounded native message history (§13 OC03)."""
        if type(vendorsid) is not str or not vendorsid.startswith('ses') or '/' in vendorsid:
            raise Blocked('invalid owned vendor session ID')
        path='/api/session/'+urllib.parse.quote(vendorsid,safe='')+'/message'
        value=self.vendor('GET',path)
        if value['status']!=200 or type(value['body']) is not dict \
                or type(value['body'].get('data')) is not list:
            raise Blocked('native history shape unavailable')
        return value['body']['data']

    def _catalog(self):
        query=urllib.parse.urlencode({'location[directory]':str(self.project)})
        status,raw=self._http.request('GET','/api/model?'+query)
        body=self._json(raw)
        if status!=200 or type(body) is not dict or type(body.get('data')) is not list:
            raise Blocked('location catalog unavailable')
        result={}
        for row in body['data']:
            _typed(row,{'providerID':S,'id':S,'cost':list},'catalog')
            identity=row['providerID']+'/'+row['id']
            free=bool(row['cost'])
            for tier in row['cost']:
                _typed(tier,{'input':(int,float),'output':(int,float),
                             'cache':{'read':(int,float),'write':(int,float)}},'catalog cost')
                free=free and all(type(value) in {int,float} and math.isfinite(value) and value==0
                                 for value in (tier['input'],tier['output'],tier['cache']['read'],tier['cache']['write']))
            result[identity]={'free':free}
        self.catalog_cache=result
        return result

    def spending_check(self,*,args=None,body=None,path=None,cost=None):
        """Recheck structure for every model-capable request, never infer zero cost."""
        self.ensure_vendor()
        status,raw=self._http.request('GET','/api/integration')
        if status!=200: raise Blocked('integration check unavailable')
        integration=self._json(raw)
        identity=safety.FREE_IDENTITY
        path_session=None
        if path:
            parts=urllib.parse.urlsplit(path).path.split('/')
            if len(parts)>=5 and parts[1:3]==['api','session']:
                path_session=parts[3]
                if not path_session.startswith('ses_'): raise Blocked('direct session identity invalid')
        if path_session:
            status,raw=self._http.request('GET','/api/session/'+path_session)
            value=self._json(raw)
            if status!=200 or type(value.get('data')) is not dict or value['data'].get('id')!=path_session:
                raise Blocked('direct session model readback unavailable')
            ref=value['data'].get('model'); _typed(ref,{'providerID':S,'id':S},'direct session model')
            identity=ref['providerID']+'/'+ref['id']
            if identity not in {safety.FREE_IDENTITY,safety.MOCK_IDENTITY}:
                self.guard.stopped=True; raise Blocked('paid direct-session identity hard stop')
            if body and type(body) is dict and 'model' in body:
                ref=body['model']; _typed(ref,{'providerID':S,'id':S},'direct requested model')
                requested=ref['providerID']+'/'+ref['id']
                if requested not in {safety.FREE_IDENTITY,safety.MOCK_IDENTITY}:
                    self.guard.stopped=True; raise Blocked('paid direct-request identity hard stop')
                if requested!=identity: raise Blocked('direct request model override differs from frozen session')
        elif args and '--model' in args: identity=args[args.index('--model')+1]
        elif body and type(body) is dict and type(body.get('model')) is dict:
            ref=body['model']; identity=ref['providerID']+'/'+ref['id']
        elif args and len(args)>1 and args[1] in self.handles:
            sid=self._vendor_sid(args[1]); status,raw=self._http.request('GET','/api/session/'+sid)
            value=self._json(raw)
            if status!=200 or type(value.get('data')) is not dict: raise Blocked('session model readback unavailable')
            ref=value['data'].get('model')
            _typed(ref,{'providerID':S,'id':S},'session model')
            identity=ref['providerID']+'/'+ref['id']
        provider,sep,model=identity.partition('/')
        if not sep: raise Blocked('model identity incomplete')
        names=self.vault.names(self.vendor_identity)
        if self.inventory: self.inventory.check()
        catalog=self._catalog()
        # Effective project config is constructed before start and immutable during turns.
        row=next((row for row in self.fixtures.values() if row['path']==self.project),None)
        endpoints={} if row is None else self._validate_provider_config(row['config'])
        if endpoints!=self.provider_endpoints:
            raise Blocked('project provider configuration drift')
        bindings=self._auxiliary_bindings(identity)
        if provider not in endpoints and not self.automatic_models_proven:
            raise Blocked('automatic/auxiliary model selection has not been proved with mock')
        if provider in endpoints:
            self._last_auxiliary_bindings=bindings
        self.last_model=identity
        return self.guard.check(integration=integration,environment_names=names,catalog=catalog,
                                provider=provider,model=model,project_providers=endpoints,cost=cost)

    def _auxiliary_bindings(self,identity):
        schema=self._served_schema()
        offered=schema.get('components',{}).get('schemas',{}).get('Config.InfoEncoded',{}).get('properties',{})
        if not {'model','agents','providers'}<=set(offered):
            raise Blocked('pinned effective selector readback schema unsupported')
        query=urllib.parse.urlencode({'location[directory]':str(self.project)})
        status,raw=self._http.request('GET','/api/config?'+query)
        value=self._json(raw)
        entries=value.get('data') if type(value) is dict else value
        if status!=200 or type(entries) is not list or not entries:
            raise Blocked('effective configuration sources unobservable')
        # Config.Entry documents are ordered low-to-high priority (E54). Never
        # persist their provider secrets; inspect only selectors and endpoints.
        effective={}; agents={}; providers={}
        for entry in entries:
            if type(entry) is not dict or entry.get('type') not in {'document','directory'}:
                raise Blocked('unknown effective configuration entry')
            if entry['type']=='directory': continue
            info=entry.get('info')
            if type(info) is not dict: raise Blocked('configuration document missing info')
            for key in ('model','default_agent'):
                if key in info: effective[key]=info[key]
            for key,merged in (('agents',agents),('providers',providers)):
                if key not in info: continue
                if type(info[key]) is not dict: raise Blocked('configuration selector shape unknown')
                for name,row in info[key].items():
                    if type(row) is not dict: raise Blocked('configuration selector entry unknown')
                    merged[name]={**merged.get(name,{}),**row}
        def selector(model):
            if type(model) is str: return model.split('#',1)[0]
            if type(model) is dict and type(model.get('providerID')) is str and type(model.get('model')) is str:
                return model['providerID']+'/'+model['model']
            raise Blocked('effective model selector unknown')
        if 'model' not in effective: raise Blocked('effective default model unavailable')
        bindings=[selector(effective['model'])]
        required={'via','title','summary','compaction','explore','general','build','plan'}
        if not required<=set(agents): raise Blocked('effective auxiliary agent selectors incomplete')
        for name,row in agents.items():
            if row.get('disabled') is True: continue
            if 'model' not in row: raise Blocked('effective auxiliary model is not frozen')
            bindings.append(selector(row['model']))
        if any(model!=identity for model in bindings):
            self.guard.stopped=True; raise Blocked('auxiliary model drift or paid identity')
        if self._validate_provider_config({'providers':providers})!=self.provider_endpoints:
            raise Blocked('effective provider endpoints differ from owned fixture')
        return bindings

    def _served_schema(self):
        """Verify §2 E7 before interpreting vendor-specific qualification routes."""
        if getattr(self,'_schema_generation',None)==self.vendor_identity:
            return self._pinned_schema
        status,raw=self._http.request('GET','/openapi.json')
        digest=hashlib.sha256(raw).hexdigest()
        if status!=200 or digest!='540fdf565da27de9df69b6c3864582344e74ac4ffa225c283b289481d215d241':
            raise Blocked('pinned served OpenAPI differs from packet E7')
        schema=self._json(raw)
        if type(schema) is not dict or type(schema.get('paths')) is not dict:
            raise Blocked('pinned served OpenAPI shape unavailable')
        self._record('openapi',{'sha256':digest,'e7_matches':True})
        self._schema_generation=self.vendor_identity; self._pinned_schema=schema
        return schema

    def admit_automatic_models(self,identities,*,mock_received,configuration_immutable):
        """Allow admission only after an observed mock proof of all auxiliary selectors."""
        safety.require_proof(mock_received,'auxiliary model proof lacks mock receipt')
        safety.require_proof(configuration_immutable,'auxiliary selectors are mutable')
        if not identities or any(value not in {safety.FREE_IDENTITY,safety.MOCK_IDENTITY} for value in identities):
            raise Blocked('auxiliary model paid or unobservable')
        self.automatic_models_proven=True

    def account(self,envelope):
        """Zero or unavailable vendor-interval cost; positive cost hard stops immediately."""
        value=envelope['cost']['usd']
        if value is not None and (type(value) not in {int,float} or not math.isfinite(value)
                                  or value<0 or value>0):
            self.guard.stopped=True; raise Blocked('positive or invalid cost hard stop')
        vendor=envelope['vendor_version']
        if vendor is not None and vendor!='2.0.22':
            self.guard.stopped=True; raise Blocked('unapproved vendor identity')

    def _vendor_sid(self,session):
        for reply in reversed(self.owned_replies):
            if reply.get('session_id')==session and type(reply.get('vendor_session_id')) is str:
                return reply['vendor_session_id']
        raise Blocked('confirmed vendor session ID missing')

    def start_event_capture(self):
        """Capture bounded owned SSE in memory for real request/action attribution."""
        self.close_event_capture()
        self._event_stop.clear(); self.events=[]; self.events_error=None
        parts=urllib.parse.urlsplit(self._http.origin)
        conn=http.client.HTTPConnection(parts.hostname,parts.port,timeout=1)
        self._event_conn=conn
        password=self._http.password
        auth=base64.b64encode(b'opencode:'+password).decode()
        self.proc.verify(self.vendor_identity)
        conn.connect(); self._event_socket=conn.sock
        ready=threading.Event(); header_deadline=time.monotonic()+SSE_HANDSHAKE_SECONDS
        stream_deadline=self.phase_deadline or time.monotonic()+120*60
        def watchdog():
            while not self._event_stop.wait(.01):
                deadline=(self.phase_deadline or stream_deadline) if ready.is_set() else header_deadline
                if time.monotonic()>=deadline:
                    self.events_error='Deadline'; break
            if self._event_socket:
                try: self._event_socket.shutdown(socket.SHUT_RDWR)
                except OSError: pass
        self._event_watchdog=threading.Thread(target=watchdog,name='owned-opencode-sse-deadline',daemon=True)
        self._event_watchdog.start()
        try:
            conn.request('GET','/api/event',headers={'Authorization':'Basic '+auth,'Accept':'text/event-stream'})
            response=conn.getresponse()
            # Normal idle SSE has no per-read timeout. The owned watchdog enforces
            # the absolute phase ceiling and shuts this saved socket down on stop.
            self._event_socket.settimeout(None)
            ready.set()
        except BaseException as error:
            self.close_event_capture()
            raise Blocked('owned event handshake failed within absolute deadline') from error
        if response.status!=200:
            response.close(); self.close_event_capture(); raise Blocked('owned native event stream unavailable')
        def consume():
            current=bytearray(); data=[]; event_id=None; total=0
            try:
                while not self._event_stop.is_set():
                    line=response.readline(1024*1024+1)
                    if not line: raise Blocked('owned event stream ended')
                    total+=len(line); current.extend(line)
                    if len(current)>1024*1024 or total>OBSERVATION_BYTES:
                        raise Blocked('owned event capture bound')
                    if line in (b'\n',b'\r\n'):
                        if data:
                            raw=b'\n'.join(data)
                            if self.vault.leaks(raw): raise Blocked('password in event stream')
                            value=self._json(raw)
                            if type(value) is not dict or type(value.get('type')) is not str:
                                raise Blocked('native event shape unavailable')
                            with self._events_lock:
                                self.events.append({'seq':len(self.events)+1,'vendor_event_id':event_id,
                                                    '_observed_at':time.monotonic(),**value})
                        current.clear(); data=[]; event_id=None
                    elif line.startswith(b'data:'): data.append(line[5:].lstrip().rstrip(b'\r\n'))
                    elif line.startswith(b'id:'): event_id=line[3:].strip().decode('utf-8','strict')
            except BaseException as error:
                if not self._event_stop.is_set(): self.events_error=type(error).__name__
            finally: response.close(); conn.close()
        self._event_thread=threading.Thread(target=consume,name='owned-opencode-events',daemon=True)
        self._event_thread.start()

    def close_event_capture(self):
        self._event_stop.set()
        if self._event_socket:
            try: self._event_socket.shutdown(socket.SHUT_RDWR)
            except OSError: pass
        if self._event_conn:
            self._event_conn.close(); self._event_conn=None
        if self._event_thread:
            self._event_thread.join(timeout=2)
            if self._event_thread.is_alive(): raise Blocked('event observer stop unverified')
            self._event_thread=None
        if self._event_watchdog:
            self._event_watchdog.join(timeout=2)
            if self._event_watchdog.is_alive(): raise Blocked('event deadline observer stop unverified')
            self._event_watchdog=None
        self._event_socket=None

    def observe(self,kind,target=None):
        """Return typed raw owned observations; unavailable observations always block."""
        if kind=='interruption':
            return {'interrupted':bool(self.signals and self.signals.interrupted)}
        if kind=='build_hashes':
            for build in ('release','failpoints'):
                if build not in self.build_hashes: self.build(build)
            return {'release':self.build_hashes['release'],'failpoints':self.build_hashes['failpoints']}
        if kind=='daemon_stop_proof':
            if self.stop_result is None: raise Blocked('daemon stop not observed')
            return self.stop_result
        if kind=='memory_sample':
            self.ensure_vendor(); raw=self.proc.read(self.vendor_identity,'status')
            fields={line.split(b':',1)[0]:line.split(b':',1)[1].strip() for line in raw.splitlines() if b':' in line}
            try: rss=int(fields[b'VmRSS'].split()[0])*1024
            except (KeyError,ValueError) as error: raise Blocked('RSS unavailable') from error
            sessions=target.get('sessions') if type(target) is dict else getattr(self,'_l9_sessions',None)
            if type(sessions) is not list or not sessions or len(sessions)>16 \
                    or any(type(sid) is not str or sid not in self.handles for sid in sessions):
                raise Blocked('RSS session-count sample lacks bounded owned session IDs')
            self._l9_sessions=list(dict.fromkeys(sessions)); active=0
            for sid in self._l9_sessions:
                status=self.via(['status',sid])
                if type(status.get('state')) is not str: raise Blocked('sampled session state unavailable')
                active+=status['state']=='running'
            return {'complete':True,'rss_bytes':rss,'at':time.monotonic(),
                    'active_sessions':active,'sessions':len(self._l9_sessions),
                    'retained_keys':None,'retained_bytes':None,'within_packet_bounds':None,
                    'retained_bounds_proof':None,'vendor_identity':self.vendor_identity.report()}
        if kind=='native_events':
            if type(target) is dict: target=target.get('session')
            if self.events_error: raise Blocked('native stream observation failed')
            if not self._event_thread: raise Blocked('native stream not observed')
            sid=self._vendor_sid(target) if isinstance(target,str) and target in self.handles else target
            with self._events_lock: events=list(self.events)
            if sid:
                events=[row for row in events if self._event_session(row)==sid or row.get('sessionID')==sid]
            return {'complete':True,'events':events}
        if kind=='owned_transcript':
            session=target['session'] if type(target) is dict else target
            sid=self._vendor_sid(session) if session in self.handles else session
            result=self.vendor('GET','/api/session/'+sid)
            if result['status']!=200 or result['body'].get('data',{}).get('id')!=sid:
                raise Blocked('owned session history existence unverified')
            return {'complete':True,'messages':self.transcript(sid),'history_exists':result['status']==200}
        if kind=='auth_checks':
            self.ensure_vendor(); status,_=self._http.request('GET','/api/info',authenticated=False)
            wrong=safety.OwnedHTTP(self._http.origin,self.vendor_identity,self.proc,b'wrong-owned-fixture')
            wrong_status,_=wrong.request('GET','/api/info')
            good,raw=self._http.request('GET','/api/info'); info=self._json(raw)
            scan=self.secrecy_scan()
            helpers=list(self.helper_ledger.values())
            if not helpers: raise Blocked('tool password absence has no captured helper evidence')
            if self._last_rotation is None: raise Blocked('second generation auth rotation not observed')
            return {'complete':True,'absent_status':status,'wrong_status':wrong_status,'correct_status':good,
                    'pid_matches':info['pid']==self.vendor_identity.pid,'memory_only':True,
                    'evidence_scan_complete':scan['complete'],'evidence_password_absent':scan['secret_absent'],
                    'tool_password_key_absent':all(row['password_key_absent'] for row in helpers),
                    'password_changed':self._last_rotation}
        if kind=='configuration_states':
            if type(target) is dict: target=target.get('session')
            if not isinstance(target,str): raise Blocked('configuration observation session missing')
            reply=self.via(['status',target])
            return {'complete':True,'inherit':reply['inherit'],'warnings':reply['warnings']}
        if kind=='permission_observations':
            if type(target) is dict: target=target.get('session')
            root_sid=self._vendor_sid(target)
            all_native=self.observe('native_events')['events']
            child_ids={row.get('data',{}).get('id',row.get('data',{}).get('sessionID'))
                       for row in all_native if row['type']=='session.created'
                       and row.get('data',{}).get('parentID')==root_sid}
            child_ids={sid for sid in child_ids if type(sid) is str}
            native=[row for row in all_native if self._event_session(row) in {root_sid,*child_ids}]
            rules=[]
            for sid in sorted(child_ids):
                result=self.vendor('GET','/api/session/'+sid)
                data=result['body'].get('data')
                if result['status']!=200 or type(data) is not dict or type(data.get('permissions')) is not list:
                    raise Blocked('actual child permission rule readback unavailable')
                rules.append({'session_id':sid,'permissions':data['permissions']})
            events=read_pages(lambda after:self.via(['events',target,'--after',str(after)]))
            return {'complete':True,'events':events,'native_events':native,'child_rules':rules}
        if kind=='helper_observations':
            folder=self._helper_folder(self.project)
            if not folder.is_dir(): raise Blocked('helper observation absent')
            values=[]
            for path in sorted(folder.glob('*.json')):
                if path.stat().st_size>OBSERVATION_BYTES: raise Blocked('helper observation bound')
                raw=path.read_bytes()
                if self.vault.leaks(raw): raise Blocked('helper wrote password')
                value=self._json(raw)
                _typed(value,{'pid':I,'start_ticks':I,'password_key_absent':B},'helper')
                identity=safety.Identity(value['pid'],value['start_ticks'])
                if self.proc.alive(identity) is True:
                    self.proc.verify(identity)
                    if identity not in self.helper_ledger and not self._descends_from(identity,self.vendor_identity):
                        raise Blocked('helper readiness did not come from owned generation')
                    self.helper_ledger[identity]=dict(value); self.helper_generations[identity]=self.vendor_identity
                    self.helper_origins[identity]=folder
                elif identity not in self.helper_ledger:
                    continue
                # Historical after-spawn state belongs only to an identity captured alive before.
                if identity in self.helper_ledger and self.proc.gone(identity) and value.get('point','').endswith('_after'):
                    self.helper_ledger[identity]=dict(value)
                values.append(self.helper_ledger[identity])
            if not values: raise Blocked('helper observations missing')
            return {'complete':True,'helpers':values}
        if kind=='marker_helper':
            # Host's marker-matched leftover report is public outcome evidence, not /proc search authority.
            helpers=self.observe('helper_observations')['helpers']
            reports=[reply['leftovers'] for reply in self.owned_replies if type(reply.get('leftovers')) is dict]
            if not reports: raise Blocked('Host leftover report unavailable')
            marker=[row for row in helpers if row.get('kind')=='marker']
            if len(marker)!=1: raise Blocked('marker helper identity ambiguous')
            helper=marker[0]; identity=safety.Identity(helper['pid'],helper['start_ticks'])
            self.proc.verify(identity)
            started=self._started_at(identity.start_ticks)
            report=reports[-1]
            _typed(report,{'scope':S,'processes':[{'pid':I,'started_at':S}],'incomplete':B},'Host leftovers')
            matches=any(row['pid']==identity.pid and row['started_at']==started for row in report['processes'])
            if not matches or report['incomplete']: raise Blocked('Host exact-marker helper proof unavailable')
            vendor_stat=self.proc.stat(self.vendor_identity.pid)
            separate=helper.get('pgid')==identity.pid
            return {'complete':True,'separate_group':separate,'host_leftovers':report,
                    'helper_identity':identity.report(),'helper_started_at':started,'marker_matches':matches}
        if kind=='seam_observation':
            point=target.get('point') if type(target) is dict else target
            return {'acknowledged':[row['occurrence'] for row in self._acknowledgements(point)]}
        if kind=='secrecy_scan': return self.secrecy_scan()
        if kind=='inventory': return {'complete':True,'unchanged':self.inventory.check()}
        if kind=='lifecycle': return self.lifecycle(target)
        return self._operation(kind,target)

    @staticmethod
    def _event_session(event):
        value=event.get('data',event)
        if type(value) is not dict: return None
        if isinstance(value.get('sessionID'),str): return value['sessionID']
        form=value.get('form')
        return form.get('sessionID') if type(form) is dict else None

    def secrecy_scan(self):
        """Gate synthetic values on VIA sinks; passwords/bearers on all readable roots (§13 L4)."""
        private_roots={self.home,*(Path(value) for key,value in self.env.items()
                                  if key in safety.PRIVATE_PARTS)}
        private_roots.update(row['path'] for row in self.fixtures.values())
        if self.namespace is not None: private_roots.add(self.namespace)
        roots={self.evidence,self.state,*private_roots}
        fixture_sources={row['path']/'opencode.json' for row in self.fixtures.values()}
        db=self.state/'store.sqlite3'
        clean=True; captures=[]; seen=set(); metadata=[]; synthetic=[]; total=0
        vendor_synthetic=0
        handles={handle.encode() for handle in self.handles.values()} | self.bearer_forms
        synthetic_forms=[form for form in self.secret_forms if form and form not in handles]
        for root in roots:
            try: root_info=root.lstat()
            except FileNotFoundError: continue
            if not stat.S_ISDIR(root_info.st_mode):
                if root not in seen: seen.add(root); metadata.append(root)
                continue
            def unreadable(_error): raise Blocked('private secrecy scan directory unreadable')
            for directory,dirs,files in os.walk(root,followlinks=False,onerror=unreadable):
                dirs[:]=[name for name in dirs if name!='.git']
                for name in dirs[:]:
                    path=Path(directory)/name
                    if not stat.S_ISDIR(path.lstat().st_mode):
                        dirs.remove(name)
                        if path not in seen: seen.add(path); metadata.append(path)
                if len(seen)>100000: raise Blocked('private secrecy scan entry bound')
                for name in files:
                    path=Path(directory)/name
                    if path in seen: continue
                    seen.add(path)
                    if len(seen)>100000: raise Blocked('private secrecy scan entry bound')
                    info=path.lstat()
                    if not stat.S_ISREG(info.st_mode): metadata.append(path); continue
                    if path==db: continue  # Own VIA Store has a consistent in-memory backup below.
                    protected_directory=any(safety.credential_metadata_only(part)
                                            for part in path.relative_to(root).parts[:-1])
                    if protected_directory or safety.credential_metadata_only(name) or name.lower().endswith('.db') \
                            or name=='opencode.json' and path not in fixture_sources:
                        # Authentication/config/database contents remain unread. Counts
                        # label this proof limit without recording their private paths.
                        metadata.append(path); continue
                    source=path in fixture_sources
                    readable=source or path.suffix.lower() in {'.json','.jsonl','.ndjson','.log','.txt'} \
                        or any(part in {'log','logs'} for part in path.parts) \
                        or any(token in name.lower() for token in ('stderr','stdout','output','undecoded'))
                    if not readable: continue
                    if info.st_size>OBSERVATION_BYTES: raise Blocked('evidence scan bound')
                    total+=info.st_size
                    if total>EVIDENCE_BYTES: raise Blocked('private secrecy scan byte bound')
                    raw=path.read_bytes()
                    protected=self.vault.leaks(raw) or any(handle in raw for handle in handles)
                    via_sink=(path.is_relative_to(self.evidence) or path.is_relative_to(self.state)) \
                        and not any(path.is_relative_to(folder) for folder in private_roots)
                    matched=any(form in raw for form in synthetic_forms)
                    if source: synthetic.append(path)
                    elif via_sink: protected=protected or matched
                    elif matched: vendor_synthetic+=1
                    clean=clean and not protected
                    if via_sink and path.name in {'undecoded.bin','stderr.log'}: captures.append(path.name)
        # Never follow a non-regular Store or state directory. All other VIA Store
        # bytes are a VIA sink, unlike the vendor's private storage above.
        if not self.state.is_symlink() and db.exists() and stat.S_ISREG(db.lstat().st_mode):
            if db.stat().st_size>EVIDENCE_BYTES: raise Blocked('Store scan bound')
            with sqlite3.connect(f'file:{db}?mode=ro',uri=True) as source:
                with sqlite3.connect(':memory:') as destination:
                    source.backup(destination)
                    raw=destination.serialize()
            clean=clean and not self.vault.leaks(raw) and not any(form in raw for form in self.secret_forms) \
                and not any(handle in raw for handle in handles)
        return {'complete':True,'secret_absent':clean,'payload_captures':len(captures),
                'metadata_only_files':len(metadata),'synthetic_source_files':len(synthetic),
                'vendor_private_synthetic_files':vendor_synthetic,
                'scope':['owned-via-store','private-vendor-logs','nested-owned-evidence'],
                'synthetic_gate_scope':'via-owned-sinks',
                'exclusions':['vendor-auth-config-and-database-content',
                              'synthetic-provider-values-in-private-fixture-config']}

    def seam(self,name,occurrence,action,*,target=None):
        """Arm one exact owned target; controller rejects unmatched generations/requests."""
        if self.phase_kind!='failpoints' or not name or type(occurrence) is not int or occurrence<1:
            raise Blocked('seam requires failpoint build and positive occurrence')
        if action not in {'pause','fail_io'}: raise Blocked('unsupported qualification seam action')
        record=self._host_record()
        if target is None: target={}
        if set(target)<= {'session','vendor_session'}:
            session=target.get('session')
            if session is None and self.last_request:
                session=self.last_request['receipt'].get('session_id')
            if name.startswith('wire.'):
                sid=target.get('vendor_session') or self._vendor_sid(session)
                target={'port':str(urllib.parse.urlsplit(self._http.origin).port),
                        'request':'/api/session/'+sid+'/prompt'}
            elif name=='routes.opencode.before_event_read':
                target={'generation':record['server_id']}
            else:
                if not session: raise Blocked('adapter seam requires known VIA session')
                status=self.via(['status',session])
                number=max(row['n'] for row in status['turns'])+1
                request='reopen' if 'reopen_identity' in name else input_id(session,number)
                target={'generation':record['server_id'],'session':session,'request':request}
        if type(target) is not dict or not target: raise Blocked('seam ownership target missing')
        command={'token':self._token,'occurrence':occurrence,'action':action,'target':target}
        directory=Path(self.env['VIA_FAILPOINT_DIR'])
        if (name,occurrence) in self._armed_history or any(
                (directory/f'{name}.{occurrence}.{suffix}').exists() for suffix in ('ack','release')):
            raise Blocked('seam occurrence cannot reuse stale ownership evidence')
        if self.vault.leaks(json.dumps(command)):
            raise Blocked('seam activation would write a protected password')
        temporary=directory/(name+'.tmp')
        fd=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_TRUNC|os.O_NOFOLLOW,0o600)
        with os.fdopen(fd,'w') as file: json.dump(command,file)
        temporary.replace(directory/(name+'.json'))
        self._armed[name]=occurrence
        self._armed_history.add((name,occurrence))
        self._seam_targets[(name,occurrence)]=dict(target)

    def seam_wait(self,name,*,timeout=30):
        if name not in self._armed: raise Blocked('seam was not armed')
        occurrence=self._armed[name]
        path=Path(self.env['VIA_FAILPOINT_DIR'])/f'{name}.{occurrence}.ack'
        deadline=time.monotonic()+timeout
        while time.monotonic()<deadline:
            if path.exists():
                raw=path.read_bytes()
                if len(raw)>4096: raise Blocked('seam acknowledgement bound')
                row=self._json(raw)
                _typed(row,{'point':S,'occurrence':I,'action':S,'pid':I},'seam acknowledgement')
                if row['point']!=name or row['occurrence']!=occurrence or not self.daemon \
                        or row['pid']!=self.daemon.pid:
                    raise Blocked('foreign seam acknowledgement')
                result={'complete':True,'owned':True,**row}
                if name.startswith('wire.'):
                    _typed(row.get('facts'),{'written':I,'body_length':I},'Wire byte facts')
                    result.update(row['facts'])
                return result
            time.sleep(0.01)
        raise Blocked('seam entry unobserved')

    def seam_release(self,name,occurrence=None):
        if name not in self._armed: raise Blocked('seam was not armed')
        directory=Path(self.env['VIA_FAILPOINT_DIR'])
        pending=[]
        for path in directory.glob(name+'.*.ack'):
            row=self._json(path.read_bytes())
            if row.get('pid')!=self.daemon.pid: raise Blocked('foreign seam release identity')
            if row.get('action')=='pause' and not (directory/f'{name}.{row["occurrence"]}.release').exists():
                pending.append(row['occurrence'])
        if not pending: raise Blocked('no entered pause remains to release')
        if occurrence is None: occurrence=min(pending)
        if (name,occurrence) not in self._armed_history or occurrence not in pending:
            raise Blocked('seam release was not armed and entered by this runner')
        fd=os.open(directory/f'{name}.{occurrence}.release',
                   os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600)
        os.close(fd)

    def _lock_holders(self,anchor):
        """Read same-uid fdinfo lock metadata only, never unrelated environments."""
        path=self.namespace/'server.lock'
        try: row=path.stat()
        except OSError as error: raise Blocked('namespace lock missing') from error
        lockid=f'{os.major(row.st_dev):02x}:{os.minor(row.st_dev):02x}:{row.st_ino}'
        holders=[]
        for directory in self.proc.root.iterdir():
            if not directory.name.isdigit(): continue
            try:
                if directory.stat().st_uid!=os.getuid(): continue
                before=self.proc.stat(int(directory.name))
                if before is None: continue
                identity=safety.Identity(before['pid'],before['start_ticks'])
                for fdinfo in (directory/'fdinfo').iterdir():
                    name='fdinfo/'+fdinfo.name
                    try: raw=self.proc.read(identity,name)
                    except (OSError,Blocked) as error:
                        cause=error if isinstance(error,OSError) else error.__cause__
                        if isinstance(cause,OSError) and cause.errno in {errno.ENOENT,errno.ESRCH} \
                                and cause.filename==name:
                            # An individual closed fd cannot retain the namespace lock.
                            # Stable process identity is still required; directory errors
                            # and hidden/reused process identities remain unverifiable.
                            self.proc.verify(identity)
                            continue
                        raise
                    if any(lockid.encode() in line and b'FLOCK' in line for line in raw.splitlines()):
                        holders.append(identity.report())
            except (OSError,Blocked):
                # Vanishing unrelated entries are harmless only when disappearance can be proved.
                if self.proc.stat(int(directory.name)) is not None:
                    raise Blocked('same-uid lock descriptor scan unverifiable')
        unique={tuple(sorted(row.items())) for row in holders}
        holders=[dict(row) for row in unique]
        if holders!=[anchor.report()]: raise Blocked('namespace lock escaped anchor')
        return holders

    def lifecycle(self,point):
        """Capture Host identity/lock facts for an explicitly reached life-point barrier."""
        self._prepare_lifecycle(point)
        self.ensure_vendor(); record=self._host_record()
        self.proc.verify(self.daemon); self.proc.verify(self.anchor); self.proc.verify(self.vendor_identity)
        status,raw=self._http.request('GET','/api/info'); info=self._json(raw)
        if status!=200 or info['pid']!=self.vendor_identity.pid: raise Blocked('vendor PID drift')
        if not self.last_request or 'session_id' not in self.last_request['receipt']:
            raise Blocked('L14 requires an active Host-backed VIA turn pin')
        active_session=self.last_request['receipt']['session_id']
        status_reply=self.via(['status',active_session])
        active=status_reply.get('active_turn')
        live_pin=type(active) is dict and active.get('phase')=='accepted' \
            and status_reply.get('state')=='running' \
            and status_reply.get('admission')=='open' and status_reply['vendor_identity_verified'] is True
        if not live_pin: raise Blocked('L14 live turn pin or no-retirement admission unverified')
        reached=False
        if point=='publication':
            rows=self.observe('native_events',active_session)['events']
            reached=self._publication_entered.is_set() and not any(
                row['type'] in {'session.tool.called','session.tool.input.started'} for row in rows)
        if point=='long_run':
            prior=getattr(self,'_long_run_sample',None)
            reached=bool(prior and prior['vendor']==self.vendor_identity and prior['completed_tail']
                         and self.proc.alive(safety.Identity(**prior['helper'])) is True)
        if point=='reload':
            operation=getattr(self,'_lifecycle_operation',None)
            reached=bool(operation and operation['point']==point and operation['status']==204
                         and operation['vendor']==self.vendor_identity)
        if not reached:
            observations=self.observe('helper_observations')['helpers']
            reached=any(value.get('point')==point and value.get('ready') is True
                        and self.helper_generations.get(safety.Identity(value['pid'],value['start_ticks']))==self.vendor_identity
                        for value in observations)
        if not reached: raise Blocked('offered life point not reached')
        sample={'point':point,'daemon':self.daemon.report(),'anchor':self.anchor.report(),
                'vendor':self.vendor_identity.report(),'lock_holders':self._lock_holders(self.anchor),
                'info_pid':info['pid'],'record_pid':record['vendor_pid'],'host_launched':True,
                'reached':reached,'live_pin':live_pin,'retirement_inflight':not live_pin,
                'build':self.phase_kind,'build_sha256':self.build_hashes[self.phase_kind]}
        sample.update(daemon_identity=sample['daemon'],anchor_identity=sample['anchor'],vendor_identity=sample['vendor'])
        self._last_lifecycle=sample
        return sample

    def _prepare_lifecycle(self,point):
        """Drive a fresh release generation to an owned fixture barrier (§13 L14)."""
        offered=getattr(self,'_lifecycle_schema',None)
        if offered is None: raise Blocked('L14 source/schema capabilities not checked')
        known={'publication','tool_during','tool_after','location_shell_during','location_shell_after',
               'session_shell_during','session_shell_after','mcp_during','mcp_after','lsp_during',
               'lsp_after','plugin_during','plugin_after','long_run','reload','disposal'}
        if point not in known: raise Blocked('unrecognized lifecycle recipe')
        if point.startswith('lsp_') and getattr(self,'_lsp_result',{}).get('spawned') is not True:
            raise Blocked('LSP lifecycle requires the positive configured-read probe')
        if point=='long_run':
            sample=getattr(self,'_long_run_sample',None)
            if not sample or sample.get('vendor')!=self.vendor_identity or not self.proc.alive(self.vendor_identity):
                raise Blocked('L14 long-run requires the retained L9 generation and final sample')
            if sample.get('completed_tail') is not True:
                raise Blocked('L9 bounded tail not completed')
            self.proc.verify(safety.Identity(**sample['helper']))
            return
        if self.daemon: self.stop()
        self.lifecycle_counter+=1
        description={'provider':'mock','helper':'tool'}
        kind=point.split('_',1)[0]
        if kind in {'mcp','plugin'}: description[kind]=True
        if kind=='lsp': description['fake_lsp']=True
        project=self.fixture('l14-'+str(self.lifecycle_counter)+'-'+point,description)
        self.start('release',project)
        if point=='publication':
            provider=self.mock_providers['l14-'+str(self.lifecycle_counter)+'-'+point]
            self._publication_entered.clear(); self._publication_release.clear()
            def publication_request(model):
                self._mock_request_admit(model); self._publication_entered.set()
                deadline=min(self.phase_deadline or float('inf'),time.monotonic()+180)
                while not self._publication_release.wait(.01):
                    if self.signals: self.signals.guard()
                    if time.monotonic()>=deadline: raise Blocked('publication response barrier deadline')
            provider.admit_request=publication_request
        receipt=self.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                          '--cwd',str(project),'--bound','full','--network','--background',
                          '--prompt','Run the local qualification tool helper and hold its barrier.'])
        if 'cli_error' in receipt: raise Blocked('fresh L14 turn refused')
        self.ensure_vendor()
        if point=='publication':
            if not self._publication_entered.wait(30): raise Blocked('publication model request unobserved')
            return
        tool=self._operation('helper_barrier',{'helper':'tool'})
        if tool.get('started') is not True or tool.get('owned') is not True:
            raise Blocked('fresh L14 tool pin unobservable')
        if point in {'publication','tool_during'}: return
        if point=='tool_after':
            # A second script step holds the live turn while the first helper's
            # observed post-spawn state supplies the after-point evidence.
            helper=self._helper_fixture(project,'completed')
            provider=self.mock_providers['l14-'+str(self.lifecycle_counter)+'-'+point]
            provider.script.append({'name':'shell','arguments':{'command':'/usr/bin/python3 '+str(helper)}})
            self._operation('helper_release',{'helper':'tool'})
            second=self._operation('helper_barrier',{'helper':'completed'})
            if second.get('owned') is not True: raise Blocked('post-tool live pin unobservable')
            path=self._helper_folder(project)/'tool.json'
            value=self._json(path.read_bytes())
            if value.get('point')!='tool_after': raise Blocked('tool completion barrier absent')
            return
        if point.startswith(('location_shell_','session_shell_')):
            helperkind='location_shell' if point.startswith('location_') else 'session_shell'
            helper=self._helper_fixture(project,helperkind)
            sid=self._vendor_sid(receipt['session_id'])
            path='/api/shell' if helperkind=='location_shell' else '/api/session/'+sid+'/shell'
            template='/api/shell' if helperkind=='location_shell' else '/api/session/{sessionID}/shell'
            body={'command':'/usr/bin/python3 '+str(helper)}
            schema=offered['paths'].get(template,{}).get('post',{}).get('requestBody',{}).get('content',{}).get('application/json',{}).get('schema')
            self._schema_body(offered,schema,body)
            if helperkind=='location_shell': path+='?'+urllib.parse.urlencode({'location[directory]':str(project)})
            errors=[]
            def request():
                try:
                    result=self.vendor('POST',path,body)
                    if result['status'] not in {200,204}: raise Blocked('L14 shell command refused')
                except BaseException as error: errors.append(error)
            worker=threading.Thread(target=request,daemon=True); worker.start()
            self._operation('helper_barrier',{'helper':helperkind})
            if point.endswith('_after'):
                self._operation('helper_release',{'helper':helperkind}); worker.join(30)
                if worker.is_alive() or errors: raise Blocked('L14 shell completion unobservable')
            else: self._lifecycle_workers=getattr(self,'_lifecycle_workers',[])+[worker]
            return
        if kind in {'mcp','lsp','plugin'}:
            helper=self._operation('helper_barrier',{'helper':kind})
            if helper.get('owned') is not True: raise Blocked('L14 configured helper ancestry unobservable')
            if point.endswith('_after'):
                self._operation('helper_release',{'helper':kind})
                deadline=time.monotonic()+30
                while time.monotonic()<deadline:
                    value=self._json((self._helper_folder(project)/(kind+'.json')).read_bytes())
                    if value.get('point')==point: return
                    time.sleep(.01)
                raise Blocked('L14 helper after-spawn point not reached')
            return
        if point=='reload':
            row=offered['paths'].get('/api/location/reload',{}).get('post')
            if type(row) is not dict or 'requestBody' in row:
                raise Blocked('pinned bodyless reload schema unavailable')
            result=self.vendor('POST','/api/location/reload')
            if result['status']!=204: raise Blocked('owned reload refused')
            self._lifecycle_operation={'point':'reload','status':result['status'],'vendor':self.vendor_identity}
            return
        raise Blocked('disposal is not offered by pinned E7')

    def _stdin_held(self,daemon,vendor):
        with self.proc.opened(vendor) as fd:
            pipe=os.readlink('fd/0',dir_fd=fd)
        if not pipe.startswith('pipe:['): raise Blocked('vendor stdin is not owned pipe')
        with self.proc.opened(daemon) as fd:
            fds=os.open('fd',os.O_RDONLY|os.O_DIRECTORY,dir_fd=fd)
            try:
                for entry in os.listdir(fds):
                    if os.readlink(entry,dir_fd=fds)!=pipe: continue
                    raw=self.proc.read(daemon,'fdinfo/'+entry)
                    lines=[line for line in raw.splitlines() if line.startswith(b'flags:')]
                    if len(lines)==1 and int(lines[0].split()[1],8)&os.O_ACCMODE in {os.O_WRONLY,os.O_RDWR}:
                        return True
            finally: os.close(fds)
        raise Blocked('stopped daemon stdin writer not proved')

    def signal_barrier(self,sample):
        """L14: one staged signal path for the live case and owned fake controls."""
        if self.phase_kind!='release': raise Blocked('L14 requires release VIA build')
        if 'operation' in sample: return self._staged_signal(sample)
        _typed(sample,{'daemon':dict,'anchor':dict,'vendor':dict,'reached':B,'live_pin':B,
                       'retirement_inflight':B},'L14 sample')
        identities={key:safety.Identity(**sample[key]) for key in ('daemon','anchor','vendor')}
        if sample['reached'] is not True or sample['live_pin'] is not True or sample['retirement_inflight'] is not False:
            raise Blocked('L14 life point not pinned or retirement uncertain')
        if identities['daemon']!=self.daemon or identities['anchor']!=self.anchor \
                or identities['vendor']!=self.vendor_identity:
            raise Blocked('L14 sample generation changed')
        for identity in identities.values(): self.proc.verify(identity)
        result={'daemon_stopped':False,'stop_identity_persisted':False,'stdin_held':False,
                'anchor_pid_only':False,'vendor_gone_within_1s':False,'daemon_resumed':False,
                'stderr_sigpipe_limitation':True}
        try:
            stopped=self.signal_barrier({'operation':'daemon_sigstop','identity':sample['daemon']})
            result.update(daemon_stopped=stopped['all_threads_stopped'],
                          stop_identity_persisted=stopped['persisted'],stdin_held=stopped['stdin_writer_open'])
            if result['stdin_held'] is not True: raise Blocked('stopped daemon stdin writer not proved')
            killed=self.signal_barrier({'operation':'anchor_sigkill','identity':sample['anchor'],'group':False})
            result['anchor_pid_only']=killed['pid_only']
            death=self.signal_barrier({'operation':'vendor_death','identity':sample['vendor'],
                                       'deadline':killed['monotonic']+1.0})
            result.update(vendor_gone_within_1s=death['gone'] and death['certain'],
                          death_elapsed_s=death['elapsed_seconds'])
        finally:
            resumed=self.signal_barrier({'operation':'daemon_sigcont','identity':sample['daemon']})
            result['daemon_resumed']=resumed['identity_verified']
            self._record('l14',result)
        return result

    def direct_seed(self,namespace,mutation):
        """Direct credential-seeding server only while private VIA stopped (§13 L11)."""
        if self.daemon is not None or Path(namespace)!=self.namespace:
            raise Blocked('L11 requires exact stopped VIA namespace')
        if not self._locks_free(): raise Blocked('L11 private VIA locks not free')
        safety.verify_binary(self.pinned)
        config=self._generated_vendor_config()
        env=dict(self.namespace_env)
        env.update(PATH=self.env['PATH'],LANG='C.UTF-8',OPENCODE_DISABLE_AUTOUPDATE='1',
                   OPENCODE_CONFIG_CONTENT=json.dumps(config,separators=(',',':')),
                   OPENCODE_PASSWORD=secrets.token_hex(32),VIA_PROCESS_MARKER=secrets.token_hex(32))
        process=subprocess.Popen([str(self.pinned),'serve','--stdio','--hostname','127.0.0.1','--port','0'],
                                 env=env,cwd=self.namespace,stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE,stderr=subprocess.DEVNULL)
        identity=None
        try:
            row=self.proc.stat(process.pid)
            if row is None: raise Blocked('L11 direct vendor identity unavailable')
            identity=safety.Identity(process.pid,row['start_ticks']); self.identities.add(identity)
            password=self.vault.read_once(self.proc,identity)
            deadline=time.monotonic()+30
            while True:
                try: origin=self.proc.listener(identity); break
                except Blocked:
                    if time.monotonic()>=deadline or process.poll() is not None: raise
                    time.sleep(.01)
            http=safety.OwnedHTTP(origin,identity,self.proc,password,seeding_mode=True)
            status,raw=http.request('GET','/api/info')
            info=self._json(raw)
            if status!=200 or info['pid']!=identity.pid or info['version']!='2.0.22':
                raise Blocked('L11 direct identity/version mismatch')
            status,raw=http.request('GET','/openapi.json')
            schema=self._json(raw)
            if status!=200: raise Blocked('L11 served schema unavailable')
            if mutation=={'schema_checked':True}:
                key_schema=schema.get('components',{}).get('schemas',{}).get('Credential.Key')
                create=schema.get('components',{}).get('schemas',{}).get('Credential.CreateInput')
                if not key_schema or not create or key_schema.get('required')!=['type','key'] \
                        or set(create.get('required',[]))!={'integrationID','value'}:
                    raise Blocked('L11 pinned synthetic key schema differs')
                secret=secrets.token_hex(32); self.secret_forms.append(secret.encode())
                mutation={'method':'POST','path':'/api/credential','body':{
                    'integrationID':'via-unused-qualification','label':'VIA synthetic fixture',
                    'value':{'type':'key','key':secret},'activate':True}}
            self._validate_seed_mutation(schema,mutation)
            status,raw=http.request(mutation['method'],mutation['path'],mutation['body'])
            if status not in {200,201,204}: raise Blocked('L11 synthetic seeding refused')
            status,raw=http.request('GET','/api/integration')
            integration=self._json(raw)
            if status!=200 or type(integration.get('data')) is not list: raise Blocked('L11 integration shape unknown')
            connected=any(row.get('connections') for row in integration['data'] if type(row) is dict)
            if not connected: raise Blocked('L11 seeded connection not observable')
            if any(form in raw for form in self.secret_forms): raise Blocked('L11 synthetic secret appeared in integration')
            result={'complete':True,'exact_namespace_layout':True,'chain_0700':True,
                    'known_connection_shape':True,'secret_absent_from_integration':True,'model_requests':0}
            # Return is deliberately after finally's positive absence proof below.
        finally:
            process.stdin.close()
            try: process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                if identity: self.proc.verify(identity); process.kill()
                process.wait(timeout=5)
            process.stdout.close()
            if identity and not self.proc.gone(identity): raise Blocked('L11 direct vendor stop unverified')
            if not self._pgrep_clear(): raise Blocked('L11 direct subtree absence unverified')
        result.update(direct_child_gone=True,descendants_gone=True,pgrep_absent=True)
        return result

    @staticmethod
    def _generated_vendor_config():
        return {'$schema':'https://opencode.ai/config.json','autoupdate':False,'share':'disabled',
                'default_agent':'via','tool_output':{'max_bytes':51200,'max_lines':2000},
                'permission':{'*':'allow','question':'deny'},
                'agent':{'via':{'mode':'primary','description':'VIA',
                                'permission':{'*':'allow','question':'deny'}}}}

    @staticmethod
    def _validate_seed_mutation(schema,mutation):
        _typed(mutation,{'method':S,'path':S,'body':dict},'L11 mutation')
        if mutation['method']!='POST' or mutation['path']!='/api/credential':
            raise Blocked('L11 only permits served-schema synthetic credential creation')
        paths=schema.get('paths')
        if type(paths) is not dict or type(paths.get('/api/credential')) is not dict \
                or 'post' not in paths['/api/credential']:
            raise Blocked('L11 credential creation not offered by pinned schema')
        spec=paths['/api/credential']['post'].get('requestBody',{}).get('content',{}).get('application/json',{}).get('schema')
        Driver._schema_body(schema,spec,mutation['body'])

    @staticmethod
    def _schema_body(document,schema,value,depth=0):
        if depth>64 or type(schema) is not dict: raise Blocked('seed schema unavailable')
        if '$ref' in schema:
            ref=schema['$ref']
            if not ref.startswith('#/components/schemas/'): raise Blocked('external seed schema refused')
            return Driver._schema_body(document,document['components']['schemas'][ref.split('/')[-1]],value,depth+1)
        if 'anyOf' in schema or 'oneOf' in schema:
            for choice in schema.get('anyOf',schema.get('oneOf',[])):
                try: Driver._schema_body(document,choice,value,depth+1); return
                except Blocked: pass
            raise Blocked('seed body matches no schema alternative')
        expected={'object':dict,'array':list,'string':str,'integer':int,'number':(int,float),
                  'boolean':bool,'null':type(None)}.get(schema.get('type'))
        if expected is None: raise Blocked('seed body schema unsupported')
        if not isinstance(value,expected) or isinstance(value,bool) and expected in {int,(int,float)}:
            raise Blocked('seed body schema type mismatch')
        if 'enum' in schema and value not in schema['enum']: raise Blocked('seed body enum mismatch')
        if type(value) is dict:
            properties=schema.get('properties',{})
            if any(key not in value for key in schema.get('required',[])): raise Blocked('seed body required field absent')
            if schema.get('additionalProperties') is False and set(value)-set(properties): raise Blocked('seed body unknown field')
            for key,inner in value.items():
                if key in properties: Driver._schema_body(document,properties[key],inner,depth+1)
        if type(value) is list:
            for inner in value: Driver._schema_body(document,schema.get('items'),inner,depth+1)

    @staticmethod
    def _lock_free(path):
        try:
            fd=os.open(path,os.O_RDWR|os.O_CLOEXEC|os.O_NOFOLLOW)
            try: fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB); return True
            except BlockingIOError: return False
            finally: os.close(fd)
        except FileNotFoundError: return True
        except OSError: return None

    def _locks_free(self): return all(self._lock_free(path) is True for path in self._lock_paths())

    def _pgrep_clear(self):
        """Targeted read-only pgrep, never signals a name/marker match."""
        rc,out,_=self.execute(['/usr/bin/pgrep','-f','--',str(self.evidence)],env=self.env,timeout=10)
        if rc not in {0,1}: return None
        if rc==1: return True
        try: pids={int(line) for line in out.splitlines()}
        except ValueError: return None
        # pgrep excludes itself. The runner may name evidence in argv and is outside owned subtree.
        pids.discard(os.getpid())
        return not pids

    def stop(self):
        """Stop all observed owned generations, then prove locks and identities absent."""
        if self.journal.path.exists(): self.journal.recover()
        self._publication_release.set()
        for name,occurrence in sorted(self._armed_history):
            directory=Path(self.env['VIA_FAILPOINT_DIR'])
            ack=directory/f'{name}.{occurrence}.ack'
            if ack.exists() and self._json(ack.read_bytes()).get('action')=='pause' \
                    and not (directory/f'{name}.{occurrence}.release').exists():
                self.seam_release(name,occurrence)
        self.close_event_capture()
        for folder in self.helper_channels.values():
            for observation in folder.glob('*.json'):
                value=self._json(observation.read_bytes())
                if type(value.get('kind')) is not str: raise Blocked('helper cleanup kind missing')
                release=folder/(value['kind']+'.release')
                if not release.exists(): release.touch(mode=0o600)
                if value['kind']=='marker':
                    (folder/'marker-parent.release').touch(mode=0o600,exist_ok=True)
        if self.daemon is not None:
            alive=self.proc.alive(self.daemon)
            if alive is None: raise Blocked('private daemon stop identity uncertain')
            if alive:
                self.proc.verify(self.daemon)
                reply=self.via(['daemon','stop','--force'])
                if reply.get('exit_code'): raise Blocked('private daemon stop refused')
        deadline=time.monotonic()+90
        while time.monotonic()<deadline:
            states=[self.proc.alive(identity) for identity in self.identities]
            locks=[self._lock_free(path) for path in self._lock_paths()]
            if all(state is False for state in states) and locks==[True,True]: break
            time.sleep(0.02)
        pgrep=self._pgrep_clear()
        safety.stop_proof(process_states=states,locks_free=locks,uncertain=self.uncertain,pgrep_clear=pgrep)
        self.stop_result={'complete':True,'proven':True,'processes_gone':True,'locks_free':True,'pgrep_clear':True}
        self._record('daemon-stop',self.stop_result)
        self.daemon=None; self.anchor=None; self._http=None
        self._armed={}; self._armed_history=set(); self._seam_targets={}; self._event_generation=None
        if self.runtime.parent==Path('/tmp') and self.runtime.name.startswith('via-ocl.'):
            shutil.rmtree(self.runtime)
        if self.pinned.exists(): safety.verify_binary(self.pinned)
        return self.stop_result

    def _materialize_fixture(self,name,project,description):
        """Create reviewed local mock/helpers from case descriptors, never install packages."""
        from opencode_cases import LoopbackProvider
        config={key:value for key,value in description.items() if key in
                {'providers','model','agents','permissions','lsp','mcp','plugins','instructions','compaction'}}
        descriptors={'provider','response','hostile_keys','context','helper','separate_group','fake_lsp',
                     'subagent','subagent_permission','skills','instructions','large_fields','script','mcp','plugin','hook','cache_read','cache_write','script_after_setup'}
        unknown=set(description)-set(config)-descriptors
        if unknown: raise Blocked('unknown fixture configuration descriptor')
        public_free=name=='public-free' or description.get('provider')=='public-free'
        if public_free:
            if not self.automatic_models_proven: raise Blocked('public phase requires completed auxiliary mock proof')
            config['model']=safety.FREE_IDENTITY
            config['agents']={agent:{'model':safety.FREE_IDENTITY}
                              for agent in ('via','title','summary','compaction','explore','general','build','plan')}
        needmock=not public_free
        if needmock and 'providers' not in config:
            markers={'local_marker':'VIA_LOCAL_AGENT_SENTINEL',
                     'ancestor_agents':'VIA_ANCESTOR_AGENTS_SENTINEL',
                     'ancestor_skills':'VIA_ANCESTOR_SKILL_SENTINEL'}
            provider=LoopbackProvider(mode=description.get('response','normal'),markers=markers,
                                      script=description.get('script',[]),cache_read=description.get('cache_read',7),
                                      cache_write=description.get('cache_write',3))
            if description.get('large_fields'):
                provider.text=('VIA escape "\\\\\\n\\t雪 '*4096)
            provider.admit_request=self._mock_request_admit
            provider.__enter__(); self.mock_providers[name]=provider
            origin=safety.loopback_origin(provider.endpoint); self.mock_origins.add(origin)
            self.guard.mock_origins=frozenset(self.mock_origins)
            cost={'input':0,'output':0,'cache':{'read':0,'write':0}}
            model={'modelID':'fixture-free','name':'VIA local fixture','cost':cost,
                   'limit':{'context':description.get('context',32768),'output':4096}}
            config['providers']={'oclive-mock':{'name':'VIA loopback mock',
                'package':'@ai-sdk/openai-compatible','env':[],
                'settings':{'baseURL':provider.endpoint,'apiKey':'fixture-unused'},
                'models':{'fixture-free':model}}}
            config['model']=safety.MOCK_IDENTITY
            config['agents']={agent:{'model':safety.MOCK_IDENTITY}
                              for agent in ('via','title','summary','compaction','explore','general','build','plan')}
            if description.get('hostile_keys'):
                secret=provider.secret
                self.secret_forms.append(secret.encode())
                providerrow=config['providers']['oclive-mock']
                providerrow['settings']['apiKey']=secret
                providerrow['headers']={'x-via-fixture':secret}
                providerrow['body']={'via-fixture':secret}
                model['headers']={'x-model-fixture':secret}; model['body']={'via-fixture':secret}
                model['variants']=[{'id':'fixture-secret','headers':{'x-variant-fixture':secret},
                                    'body':{'via-fixture':secret}}]
                providerrow['settings']['baseURL']=provider.endpoint.rstrip('/')+'/token/'+secret
                config['share']='auto'
                config['permissions']=[{'action':'*','resource':'*','effect':'ask'}]
                config['agents']['via']['permissions']=list(config['permissions'])
        if description.get('subagent'):
            config.setdefault('agents',{})[description['subagent']]={
                'mode':'subagent','model':safety.MOCK_IDENTITY,'description':'VIA qualification child',
                'permissions':[{'action':'shell','resource':'*','effect':'ask'}]}
        helper_kind=description.get('helper')
        if helper_kind is None and name in {'usage-cache','transport-loss'}: helper_kind='tool'
        if helper_kind:
            helperpath=self._helper_fixture(project,helper_kind,description.get('separate_group',False))
            if public_free and helper_kind:
                with (project/'AGENTS.md').open('a') as instructions:
                    instructions.write('\nThe qualification helper command is: /usr/bin/python3 .opencode/'+helper_kind+'-helper.py\n')
            if needmock and name in self.mock_providers and helper_kind:
                provider=self.mock_providers[name]
                toolcall={'name':'shell','arguments':{'command':'/usr/bin/python3 '+str(helperpath)}}
                provider.script=([None] if description.get('script_after_setup') else [])+[toolcall]
                if name=='usage-cache':
                    release=self._helper_folder(project)/'tool.release'
                    release.touch(mode=0o600)
        if description.get('fake_lsp'):
            helper=self._helper_fixture(project,'lsp')
            config['lsp']={'via-fixture':{'command':['/usr/bin/python3',str(helper)],
                                        'extensions':['.via_ocl_fixture'],'disabled':False}}
            (project/'fixture.via_ocl_fixture').write_text('VIA fixture\n')
        if description.get('mcp') is True:
            helper=self._helper_fixture(project,'mcp')
            config['mcp']={'servers':{'via-fixture':{'type':'local','command':['/usr/bin/python3',str(helper)],
                           'timeout':{'startup':10000,'catalog':10000,'execution':10000}}}}
        if description.get('plugin') or description.get('hook'):
            plugins=safety.private_directory(project/'.opencode'/'plugins')
            directory=safety.private_directory(plugins/'via-fixture')
            plugin_helper=self._helper_fixture(project,'plugin')
            hook_helper=self._helper_fixture(project,'hook') if description.get('hook') else None
            # Pinned v2.0.22 promise/plugin.ts defines {id,setup}; an absolute
            # directory/index.js is accepted by core/plugin/module.ts without npm.
            code="import { spawn } from 'node:child_process';\nexport default {\n" \
                +" id: 'via-private-fixture',\n async setup(ctx) {\n" \
                +"  const children=[];\n" \
                +"  const start=(path)=>{ const child=spawn('/usr/bin/python3',[path],{stdio:'ignore'}); children.push(child); };\n" \
                +"  start("+json.dumps(str(plugin_helper))+");\n"
            if hook_helper:
                code+="  await ctx.tool.hook('execute.before', async () => { start("+json.dumps(str(hook_helper))+"); });\n"
            code+="  return () => { for (const child of children) { if (child.exitCode===null) child.kill('SIGTERM'); } };\n }\n};\n"
            (directory/'index.js').write_text(code); (directory/'index.js').chmod(0o400)
            config['plugins']=[str(directory)]
        if description.get('skills')=='off':
            skill=project/'.claude'/'skills'/'private-fixture'
            skill.mkdir(parents=True,mode=0o700)
            (skill/'SKILL.md').write_text('---\nname: private-fixture\ndescription: VIA sentinel\n---\nVIA_PRIVATE_SKILL\n')
        if name=='long-run': self._helper_fixture(project,'tool',barrier_seconds=1800)
        if description.get('subagent') and name in self.mock_providers:
            # The real task-tool schema is checked by LoopbackProvider before
            # returning this call. An unavailable tool cannot become evidence.
            self.mock_providers[name].script=[
                {'name':'subagent','arguments':{'description':'VIA private permission qualification',
                                           'agent':description['subagent'],
                                           'prompt':'Attempt the project ask-rule shell action.'}},
                {'name':'shell','arguments':{'command':'/usr/bin/true'}}]
        if description.get('fake_lsp') and name in self.mock_providers and (
                name=='lsp-probe' or name.endswith(('lsp_during','lsp_after')) or
                (name=='never-ask' and getattr(self,'_lsp_result',{}).get('spawned') is True)):
            self.mock_providers[name].script.insert(0,{'fixture_read':'fixture.via_ocl_fixture'})
        return config

    def _helper_fixture(self,project,kind,separate_group=False,barrier_seconds=180):
        """Local helper exposes bounded readiness/release and Boolean password absence."""
        if type(barrier_seconds) is not int or not 1<=barrier_seconds<=1800:
            raise Blocked('helper barrier exceeds reviewed phase bound')
        if type(kind) is not str or not kind.replace('_','').isalnum():
            raise Blocked('unsafe helper kind')
        observations=safety.private_directory(project/'.opencode'/'observations')
        self.helper_channels[str(project)]=observations
        path=project/'.opencode'/f'{kind}-helper.py'
        code='''#!/usr/bin/python3
import json, os, pathlib, time, sys, threading
kind=KIND
if kind=='marker':
    pid=os.fork()
    if pid:
        end=time.monotonic()+BARRIER_SECONDS
        while not (pathlib.Path(OBSERVATIONS)/'marker-parent.release').exists() and time.monotonic()<end:
            time.sleep(.01)
        sys.exit(0)
if SEPARATE:
    os.setsid()
root=pathlib.Path(OBSERVATIONS)
raw=pathlib.Path('/proc/self/stat').read_bytes()
ticks=int(raw[raw.rindex(b')')+2:].split()[19])
value={'pid':os.getpid(),'start_ticks':ticks,'kind':kind,'point':kind+'_during',
       'ready':True,'password_key_absent':'OPENCODE_PASSWORD' not in os.environ,
       'pgid':os.getpgrp()}
path=root/(kind+'.json')
def persist():
    temporary=root/(kind+'.'+str(os.getpid())+'.tmp')
    temporary.write_text(json.dumps(value)); temporary.chmod(0o600)
    temporary.replace(path)
persist()
if kind in {'lsp','mcp'}:
    def released():
        end=time.monotonic()+BARRIER_SECONDS
        while not (root/(kind+'.release')).exists() and time.monotonic()<end:
            time.sleep(.01)
        value['point']=kind+'_after'
        persist()
    threading.Thread(target=released,daemon=True).start()
if kind=='mcp':
    for line in sys.stdin.buffer:
        request=json.loads(line)
        if 'id' not in request: continue
        method=request.get('method')
        if method=='initialize':
            result={'protocolVersion':request.get('params',{}).get('protocolVersion','2024-11-05'),
                    'capabilities':{'tools':{}},'serverInfo':{'name':'via-private-fixture','version':'1'}}
        elif method=='tools/list':
            result={'tools':[{'name':'fixture','description':'VIA local MCP fixture',
                             'inputSchema':{'type':'object','properties':{},'additionalProperties':False}}]}
        elif method=='tools/call': result={'content':[{'type':'text','text':'VIA MCP READY'}]}
        elif method=='ping': result={}
        else:
            sys.stdout.write(json.dumps({'jsonrpc':'2.0','id':request['id'],
                                        'error':{'code':-32601,'message':'unsupported'}})+'\\n')
            sys.stdout.flush(); continue
        sys.stdout.write(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result})+'\\n')
        sys.stdout.flush()
elif kind=='lsp':
    while True:
        line=sys.stdin.buffer.readline()
        if not line: break
        if line.lower().startswith(b'content-length:'):
            size=int(line.split(b':')[1]); sys.stdin.buffer.readline()
            request=json.loads(sys.stdin.buffer.read(size))
            if 'id' in request:
                reply={'jsonrpc':'2.0','id':request['id'],'result':{'capabilities':{}}}
                data=json.dumps(reply).encode()
                sys.stdout.buffer.write(b'Content-Length: '+str(len(data)).encode()+b'\\r\\n\\r\\n'+data)
                sys.stdout.buffer.flush()
            if request.get('method')=='exit': break
else:
    end=time.monotonic()+BARRIER_SECONDS
    while not (root/(kind+'.release')).exists() and time.monotonic()<end:
        time.sleep(.01)
value['point']=kind+'_after'; value['ready']=True
persist()
print('VIA HELPER DONE')
'''
        code=code.replace('KIND',repr(kind)).replace('SEPARATE',repr(separate_group)).replace('OBSERVATIONS',repr(str(observations)))
        code=code.replace('BARRIER_SECONDS',str(barrier_seconds))
        path.write_text(code); path.chmod(0o500)
        return path

    def _helper_folder(self,project):
        if project is None or str(project) not in self.helper_channels:
            raise Blocked('fixture helper observation channel unavailable')
        return self.helper_channels[str(project)]

    def _staged_signal(self,sample):
        """Staged L14 API keeps the persisted barrier active across independent polls."""
        operation=sample['operation']
        identity=safety.Identity(**sample['identity'])
        if operation=='daemon_sigstop':
            if identity!=self.daemon: raise Blocked('foreign daemon barrier')
            deadline=time.monotonic()+5
            self._sigkill_at=None
            self.journal.stop(identity)
            try:
                taskroot=self.proc.root/str(identity.pid)/'task'
                while True:
                    self.proc.verify(identity)
                    rows=list(taskroot.iterdir())
                    if not rows: raise Blocked('daemon thread-group unavailable')
                    stopped=True
                    for thread in rows:
                        try:
                            raw=(thread/'stat').read_bytes()
                        except FileNotFoundError:
                            # A worker may exit before the group stop settles; rescan the group.
                            stopped=False
                            continue
                        parsed=self.proc.parse_stat(raw,int(thread.name))
                        stopped=stopped and parsed['state'] in {'T','t'}
                    if time.monotonic()>=deadline:
                        raise Blocked('daemon thread-group stop deadline exhausted')
                    if stopped: break
                    time.sleep(.005)
                held=self._stdin_held(identity,self.vendor_identity)
            except BaseException:
                self.journal.resume(identity); raise
            return {'persisted':True,'all_threads_stopped':True,'stdin_writer_open':held}
        if operation=='anchor_sigkill':
            if identity!=self.anchor or sample.get('group') is not False:
                raise Blocked('anchor-only signal authority absent')
            self.proc.verify(self.daemon)
            row=self.proc.stat(self.daemon.pid)
            if not row or row['start_ticks']!=self.daemon.start_ticks or row['state'] not in {'T','t'}:
                raise Blocked('daemon barrier not held')
            self.proc.verify(identity)
            self._sigkill_at=time.monotonic(); self.journal._signal(identity,signal.SIGKILL)
            return {'pid_only':True,'monotonic':float(self._sigkill_at)}
        if operation=='vendor_death':
            if identity!=self.vendor_identity or self._sigkill_at is None:
                raise Blocked('vendor death sample not owned')
            deadline=sample['deadline']
            if deadline>self._sigkill_at+1.000001: raise Blocked('death window exceeds one second')
            while time.monotonic()<=deadline:
                row=self.proc.stat(self.daemon.pid)
                if not row or row['start_ticks']!=self.daemon.start_ticks or row['state'] not in {'T','t'}:
                    raise Blocked('daemon resumed during death proof')
                if self.proc.gone(identity):
                    return {'gone':True,'certain':True,'daemon_stayed_stopped':True,
                            'elapsed_seconds':float(time.monotonic()-self._sigkill_at)}
                time.sleep(0.005)
            raise Blocked('vendor survived anchor or death evidence uncertain')
        if operation=='daemon_sigcont':
            if identity!=self.daemon: raise Blocked('foreign daemon continuation')
            self.journal.resume(identity)
            deadline=time.monotonic()+5
            while time.monotonic()<deadline:
                self.proc.verify(identity)
                row=self.proc.stat(identity.pid)
                if row and row['start_ticks']==identity.start_ticks and row['state'] not in {'T','t'}:
                    return {'identity_verified':True}
                time.sleep(.005)
            raise Blocked('owned daemon continuation unverified')
        raise Blocked('unknown L14 barrier operation')

    def _operation(self,kind,target):
        """Execute bounded control operations from owned evidence, with no success defaults."""
        args=target if type(target) is dict else {'session':target}
        session=args.get('session')
        if kind=='seam_target':
            record=self._host_record(); point=args['point']
            if point.startswith('wire.'):
                return {'port':str(urllib.parse.urlsplit(self._http.origin).port),
                        'request':'/api/session/'+args['vendor_session']+'/prompt'}
            if point=='routes.opencode.before_event_read': return {'generation':record['server_id']}
            request='reopen' if 'reopen_identity' in point else input_id(args['session'],args['turn_number'])
            return {'generation':record['server_id'],'session':args['session'],'request':request}
        if kind=='phase_budget':
            self.set_phase_budget(args['public_turns'],args['mock_turns'])
            return {'configured':True}
        if kind=='set_inherit':
            settings=args['settings']
            allowed={'agents','hooks','instruction_files','mcp_servers','plugins','skills'}
            if type(settings) is not dict or set(settings)-allowed or any(value not in {'on','off'} for value in settings.values()):
                raise Blocked('invalid inherited-configuration request')
            if self.daemon: self.stop()
            self.inherit_settings={key:value=='on' for key,value in settings.items()}
            return {'requested':dict(settings),'daemon_restart_required':True}
        if kind=='server_loss_for_marker':
            if not self.daemon or not self.anchor: raise Blocked('marker server-loss identities absent')
            status=self.via(['status',session])
            active=status.get('active_turn')
            if type(active) is not dict or active.get('phase')!='accepted' or status.get('state')!='running':
                raise Blocked('marker loss requires an active accepted VIA turn')
            self.proc.verify(self.anchor); self.journal._signal(self.anchor,signal.SIGKILL)
            return {'anchor_pid_only':True}
        if kind=='helper_stop_proof':
            helper=args['helper']
            rows=[(identity,row) for identity,row in self.helper_ledger.items() if row.get('kind')==helper]
            if not rows: raise Blocked('helper identity was never captured')
            deadline=time.monotonic()+30
            while time.monotonic()<deadline and any(self.proc.alive(identity) is not False for identity,_ in rows):
                time.sleep(.01)
            gone=all(self.proc.gone(identity) for identity,_ in rows)
            path=str(self.project/'.opencode'/f'{helper}-helper.py')
            rc,out,_=self.execute(['/usr/bin/pgrep','-f','--',path],env=self.env,timeout=5)
            absent=rc==1 and not out
            if not gone or not absent: raise Blocked('helper stop/pgrep absence unverified')
            return {'gone':gone,'pgrep_absent':absent}
        if kind=='retire_for_leftover_report':
            return self._operation('idle_retirement',args)
        if kind=='namespace':
            self.stop()
            self.state=safety.private_directory(self.evidence/'credential-state')
            self.namespace,self.namespace_env=safety.create_namespace(self.state)
            safety.init_fixture_repo(self.namespace,self.home)
            self.env['VIA_STATE_DIR']=str(self.state)
            self._refresh_inventory()
            return str(self.namespace)
        if kind=='fresh_max_steps': return self._fresh_max_steps(args['max_steps'])
        if kind=='long_run_barrier_prepare':
            if args.get('barrier_seconds')!=1800 or args.get('helper')!='tool' \
                    or Path(args['project'])!=self.project:
                raise Blocked('long-run barrier outside reviewed fixture')
            path=self.project/'.opencode'/'tool-helper.py'
            if not path.is_file(): raise Blocked('pre-created long-run helper absent')
            provider=next((p for name,p in self.mock_providers.items() if self.fixtures[name]['path']==self.project),None)
            if provider is None: raise Blocked('long-run mock provider absent')
            provider.script.append({'name':'shell','arguments':{'command':'/usr/bin/python3 '+str(path)}})
            return {'prepared':True,'helper':'tool','barrier_seconds':1800}
        if kind=='repository_sentinel': return self._repository_sentinel()
        if kind in {'idle_retirement','daemon_restart'}:
            sid=self._vendor_sid(session)
            if kind=='daemon_restart':
                self.stop(); self.start(self.phase_kind or 'release',self.project)
            else:
                identity=self.vendor_identity
                if identity is None: raise Blocked('idle predecessor identity absent')
                self.close_event_capture()
                # Idle retirement is a vendor route policy; do not manufacture it by signalling.
                deadline=time.monotonic()+90
                while time.monotonic()<deadline and self.proc.alive(identity) is not False:
                    time.sleep(.05)
                if not self.proc.gone(identity): raise Blocked('idle retirement not reached in bound')
                self._http=None; self._event_generation=None; self._idle_retired=True
            return {'stop_proven':True,'vendor_session_id':sid}
        if kind=='via_events':
            events=read_pages(lambda after:self.via(['events',session,'--after',str(after)]))
            terminal=[row for row in events if row['type'] in {'turn.completed','turn.failed','turn.cancelled','turn.unknown','turn.terminal'}]
            if not terminal or type(terminal[-1].get('revision')) is not int:
                raise Blocked('terminal event revision unavailable')
            return {'complete':True,'events':events,'terminal_revision':terminal[-1]['revision']}
        if kind=='turn_identity':
            envelope=args['envelope']; sid=envelope['vendor_session_id']
            native=self.observe('native_events',sid)['events']
            delivered=[row for row in native if row['type']=='session.inbox.delivered']
            terminals=[row for row in native if row['type'] in {'session.execution.succeeded','session.execution.failed','session.execution.interrupted'}]
            inputid=input_id(session,envelope['turn'])
            matching=[row for row in delivered if row.get('data',row).get('inboxID',row.get('data',row).get('id'))==inputid]
            if len(matching)!=1: raise Blocked('exact current owned input delivery unobservable')
            last=matching[0]
            # Native execution lifecycle is session-scoped (§7.1), not universally executionID-bearing.
            later=[row for row in delivered if row['seq']>last['seq']]
            boundary=min((row['seq'] for row in later),default=float('inf'))
            owned=[row for row in terminals if last['seq']<row['seq']<boundary]
            execution=inputid
            model=self._http.request('GET','/api/session/'+sid)
            ref=self._json(model[1])['data']['model']; identity=ref['providerID']+'/'+ref['id']
            if identity not in {safety.FREE_IDENTITY,safety.MOCK_IDENTITY}:
                self.guard.stopped=True; raise Blocked('paid model readback hard stop')
            return {'mode':'mock' if ref['providerID'] in self.provider_endpoints else 'public-free',
                    'model':identity,'owned_terminal':bool(owned),'execution_id':execution,'input_id':inputid,
                    'vendor_error_observed':any(row['type']=='session.step.failed' for row in native),
                    'automatic_compaction':any(row['type'].startswith('session.compaction.') for row in native)}
        if kind=='native_terminal':
            sid=self._vendor_sid(session)
            rows=self.observe('native_events',sid)['events']
            terminal=[row for row in rows if row['type'].startswith('session.execution.') and row['type']!='session.execution.started']
            if not terminal: raise Blocked('native terminal absent')
            row=terminal[-1]; data=row.get('data',row)
            return {'type':row['type'],'reason':data.get('reason'),'owned':any(event['type']=='session.inbox.delivered' and event['seq']<row['seq']
                                and str(event.get('data',event).get('inboxID','')).startswith('msg_via') for event in rows)}
        if kind=='mock_receipt':
            row=next((provider for name,provider in self.mock_providers.items()
                      if self.fixtures[name]['path']==self.project),None)
            if row is None: raise Blocked('owned mock provider absent')
            receipt=row.receipt()
            if not receipt['received'] or not receipt['model_matches']:
                self.guard.stopped=True; raise Blocked('overridden provider failed mock receipt')
            return receipt
        if kind=='helper_barrier':
            helper=args['helper']; path=self._helper_folder(self.project)/(helper+'.json')
            deadline=time.monotonic()+30
            while time.monotonic()<deadline:
                if path.exists():
                    if path.stat().st_size>OBSERVATION_BYTES: raise Blocked('helper observation bound')
                    row=self._json(path.read_bytes()); identity=safety.Identity(row['pid'],row['start_ticks'])
                    self.proc.verify(identity)
                    if row.get('ready') is True:
                        self.identities.add(identity); self.helper_ledger[identity]=dict(row); self.helper_generations[identity]=self.vendor_identity
                        self.helper_origins[identity]=path.parent
                        return {'started':True,'owned':self._descends_from(identity,self.vendor_identity),**row}
                time.sleep(.01)
            raise Blocked('helper barrier not reached')
        if kind=='helper_release':
            path=self._helper_folder(self.project)/(args['helper']+'.release')
            fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.close(fd)
            if args['helper']=='marker':
                parent=self._helper_folder(self.project)/'marker-parent.release'
                fd=os.open(parent,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.close(fd)
            return {'released':True}
        if kind=='cancel_timing':
            if self.last_cancel is None: raise Blocked('cancel not observed')
            held=[]
            for name,occurrence in self.last_cancel['held'].items():
                directory=Path(self.env['VIA_FAILPOINT_DIR'])
                if (directory/f'{name}.{occurrence}.ack').exists() and not (directory/f'{name}.{occurrence}.release').exists(): held.append(name)
            native=self.observe('native_events',self._vendor_sid(session))['events']
            interrupts=[row for row in native if row['type']=='session.execution.interrupted' and row.get('data',row).get('reason')=='user']
            proof=self._fake_control('qualification_body_prefix_cancel_preserves_offset_and_stop_pool')
            return {'cancel_returned':type(self.last_cancel['reply'].get('state')) is str,
                    'cancel_ms':self.last_cancel['elapsed_ms'],'prompt_held':bool(held),
                    'native_user_interrupted':bool(interrupts),
                    'sourcebound_pool_test':proof}
        if kind=='write_observation':
            events=self.observe('native_events',self._vendor_sid(session))['events']
            number=args.get('turn_number')
            if type(number) is not int: raise Blocked('write observation current turn unavailable')
            expected=input_id(session,number)
            enqueued=[row for row in events if row['type']=='session.inbox.enqueued'
                      and row.get('data',row).get('inboxID',row.get('data',row).get('id'))==expected]
            # Prompt submission count is measured by exact targeted Wire acknowledgement;
            # second occurrence guard must remain armed to detect a retry.
            context=args.get('context')
            if type(context) is not dict: raise Blocked('write prompt ownership context unavailable')
            target=self.observe('seam_target',{'point':args['point'],**context})
            acks=[row for row in self._acknowledgements(args['point']) if row['_target']==target]
            if not acks: raise Blocked('written request observation absent')
            reply=self.via(['wait',self.last_request['receipt']['turn'],'--timeout-ms','180000'])
            return {'enqueues':len(enqueued),'submits':len(acks),'indeterminate':reply['state']=='unknown',
                    'drained':self.proc.gone(self.vendor_identity)}
        if kind=='foreign_prompt':
            sid=self._vendor_sid(session); inputid='msg_'+secrets.token_hex(12)
            result=self.vendor('POST','/api/session/'+sid+'/prompt',{'id':inputid,'text':args['text']})
            self._foreign={'id':inputid,'sid':sid,'status':result['status']}
            return {'status':result['status']}
        if kind in {'foreign_observation','foreign_fence_observation'}:
            return self._foreign_observation(session,args,held=kind=='foreign_fence_observation')
        if kind=='loss_observation':
            events=read_pages(lambda after:self.via(['events',session,'--after',str(after)]))
            number=args.get('turn_number')
            if type(number) is not int: raise Blocked('loss current turn number missing')
            current=[row for row in events if row.get('turn')==number]
            accepted=any(row['type']=='turn.accepted' for row in current)
            terminal=any(row['type'] in {'turn.completed','turn.failed','turn.cancelled'} for row in current)
            return {'accepted_before_loss':accepted,'via_terminal_admitted':terminal}
        if kind=='seam_observation':
            return {'acknowledged':[row['occurrence'] for row in self._acknowledgements(args['point'])]}
        if kind=='missing_vendor_session': return self._missing_vendor_session(session)
        if kind=='identity_duplicate_control':
            # The duplicate-sensitive seam is proved by its fake-first Rust regression,
            # whose result is supplied by the runner's verified gate manifest, not invented live.
            name='oc_live_reopen_identity_guard_excludes_settings_and_detects_duplicate'
            proof=self._fake_control(name)
            return {'second_occurrence_failed':proof['verified'],'source':'bound fake-first gate manifest'}
        if kind=='lifecycle_capabilities': return self._lifecycle_capabilities()
        if kind=='lifecycle_successor':
            old=self.vendor_identity
            predecessor=safety.Identity(**args['predecessor'])
            if not self.proc.gone(predecessor): raise Blocked('predecessor not gone before successor')
            self._publication_release.set()
            helpers=[identity for identity in self.helper_ledger
                     if self.helper_generations.get(identity)==predecessor]
            for identity in helpers:
                alive=self.proc.alive(identity)
                if alive is None: raise Blocked('predecessor helper identity unverifiable')
                if alive is False: continue
                self.proc.verify(identity)
                folder=self.helper_origins.get(identity)
                name=self.helper_ledger[identity].get('kind')
                if folder is None or type(name) is not str or not name.replace('-','').replace('_','').isalnum():
                    raise Blocked('predecessor helper release channel unavailable')
                for channel in (name,'marker-parent') if name=='marker' else (name,):
                    path=folder/(channel+'.release')
                    if not path.exists():
                        fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.close(fd)
            deadline=time.monotonic()+HELPER_STOP_SECONDS
            while not all(self.proc.gone(identity) for identity in helpers):
                if self.signals: self.signals.guard()
                if time.monotonic()>=deadline:
                    raise Blocked('predecessor helper survives cooperative release')
                time.sleep(.01)
            self.close_event_capture(); self._http=None; self.ensure_vendor()
            record=self._host_record()
            predecessors=[identity for identity in self.server_identities if identity!=self.vendor_identity]
            parallel=sum(self.proc.alive(identity) is True for identity in predecessors)
            if any(self.proc.alive(identity) is None for identity in predecessors):
                raise Blocked('parallel-server absence unverifiable')
            return {'admitted':self.vendor_identity!=predecessor,'parallel_servers':parallel,
                    'password_changed':self.vault.changed(old,self.vendor_identity),'record':record['phase']}
        if kind=='memory_tail':
            final=None
            beginning=time.monotonic()
            deadline=min(args['deadline'],self.phase_deadline or args['deadline'])
            while time.monotonic()<deadline:
                if self.signals: self.signals.guard()
                if deadline-time.monotonic()>1:
                    final=self.observe('memory_sample',{'sessions':args.get('sessions',getattr(self,'_l9_sessions',None))})
                delay=min(args['interval_seconds'],max(0,deadline-time.monotonic()))
                if delay: time.sleep(delay)
            if final is None: final=self.observe('memory_sample')
            events=self.observe('native_events')['events']
            large=any(len(json.dumps(row).encode())>=50000 for row in events)
            if self.last_request is None: raise Blocked('L9 final sample has no owned session')
            self._long_run_sample={'vendor':self.vendor_identity,'sample':final,
                    'session':self.last_request['receipt']['session_id'],
                    'duration_seconds':float(final['at']-getattr(self,'_long_run_started',beginning)),
                    'completed_tail':time.monotonic()>=deadline}
            helpers=[identity for identity,row in self.helper_ledger.items() if row.get('kind')=='tool'
                     and self.helper_generations.get(identity)==self.vendor_identity and self.proc.alive(identity) is True]
            if len(helpers)!=1: raise Blocked('L9 final held helper identity unverified')
            self._long_run_sample['helper']=helpers[0].report()
            return {'complete':True,'final_sample':True,'large_fields_seen':large,
                    'public_requests':self._phase_public_used,'sample':final,
                    'duration_seconds':self._long_run_sample['duration_seconds'],
                    'observed_session_count':len(self._l9_sessions)}
        if kind=='bounded_probe': return self._bounded_probe(args['item'],args['attempt_limit'])
        if kind=='credential_refusal': return self._credential_refusal()
        if kind=='near_limit_inbox': return self._near_limit_inbox(args)
        raise Blocked('unknown qualification observation')

    def _acknowledgements(self,point):
        if 'VIA_FAILPOINT_DIR' not in self.env: raise Blocked('failpoint directory absent')
        rows=[]
        for path in sorted(Path(self.env['VIA_FAILPOINT_DIR']).glob(point+'.*.ack')):
            row=self._json(path.read_bytes())
            if row.get('pid')!=self.daemon.pid: raise Blocked('foreign seam evidence')
            key=(point,row.get('occurrence'))
            if key not in self._armed_history or key not in self._seam_targets:
                raise Blocked('unarmed seam acknowledgement evidence')
            row['_target']=self._seam_targets[key]
            rows.append(row)
        return rows

    def _fake_control(self,name):
        value=self.fake_gate_manifest
        _typed(value,{'verified':B,'via_builds':{'release':S,'failpoints':S},
                       'source_sha256':S,'tests':[S]},'bound fake gate manifest')
        if value['verified'] is not True or value['via_builds']!=self.build_hashes:
            raise Blocked('fake evidence belongs to another VIA build')
        if name not in value['tests'] or len(value['source_sha256'])!=64:
            raise Blocked('required fake control did not pass on current Rust source')
        return {'verified':value['verified'],'test_name':name,
                'source_sha256':value['source_sha256'],'via_builds':dict(value['via_builds'])}

    def _descends_from(self,identity,ancestor):
        if ancestor is None: return False
        self.proc.verify(identity); self.proc.verify(ancestor)
        pid=identity.pid; seen=set()
        for _ in range(1000):
            if pid in seen: raise Blocked('process ancestry cycle')
            seen.add(pid)
            if pid==ancestor.pid: return True
            row=self.proc.stat(pid)
            if row is None: raise Blocked('process ancestry vanished')
            pid=row['ppid']
            if pid<=1: return False
        raise Blocked('process ancestry bound')

    def finish(self):
        """Final cleanup/immutable-pin proof; uncertainty retains roots and fails closure."""
        try: proof=self.stop()
        finally:
            self._publication_release.set()
            failures=[]
            for provider in self.mock_providers.values():
                try: provider.__exit__(None,None,None)
                except Exception as error: failures.append(error)
            self.mock_providers.clear()
            if failures: raise Blocked('owned mock provider cleanup unverified') from failures[0]
        safety.verify_binary(self.pinned)
        scan=self.secrecy_scan()
        if scan['secret_absent'] is not True or scan['payload_captures']!=0:
            raise Blocked('final secrecy scan failed')
        return {'stopped':True,**proof,'pinned_rehash':True,'secrecy_scan':scan}

    def clear_sensitive(self):
        """Clear memory only after the entry's final protected evidence sink (§2.3)."""
        self.vault.clear(); self.secret_forms.clear(); self.handles.clear(); self.bearer_forms.clear()

    def _fresh_max_steps(self,value):
        """Refuse non-null max_steps before any vendor metadata acquisition (§13 OC11)."""
        if self.vendor_identity or self._http or self.identities:
            raise Blocked('max_steps preflight requires a fresh daemon')
        project=self.fixture('fresh-max-steps',{})
        self.start('release',project)
        before=self._anchor_rows()
        probe=self.namespace.parent/'probe'
        before_probe=self._tree_identity(probe)
        description=self.via(['describe','--harness','opencode','--model',safety.MOCK_IDENTITY,
                              '--cwd',str(project),'--bound','full','--network'])
        support=description['capabilities']['params']['max_steps']['support']
        refused=self.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                          '--cwd',str(project),'--bound','full','--network','--max-steps',str(value),
                          '--prompt','VIA preflight','--background'])
        error=refused.get('cli_error',{}).get('error',{})
        _typed(error,{'code':I,'data':{'kind':S,'field':S}},'max_steps refusal')
        if error['data']['kind']!='invalid_params' or error['data']['field']!='max_steps':
            raise Blocked('max_steps did not refuse before acquisition')
        if self._anchor_rows()!=before or self._tree_identity(probe)!=before_probe:
            raise Blocked('preflight caused a Host acquisition/version probe')
        return {'support':support,'class':error['data']['kind'],'vendor_launches':0,
                'version_probes':0,'session_creates':0,'prompt_submits':0,'watch_before':True}

    def _anchor_rows(self):
        try:
            with sqlite3.connect(f'file:{self.state / "store.sqlite3"}?mode=ro',uri=True) as conn:
                return conn.execute('SELECT anchor_id,generation,phase,vendor_pid FROM anchors').fetchall()
        except sqlite3.Error as error: raise Blocked('Host journal observation unavailable') from error

    @staticmethod
    def _tree_identity(root):
        if not root.exists(): return None
        rows=[]
        for path in sorted(root.rglob('*')):
            info=path.lstat()
            rows.append((str(path.relative_to(root)),info.st_ino,info.st_size,info.st_mtime_ns))
        return rows

    def _mock_turn(self,project,prompt):
        self.start(self.phase_kind or 'release',project)
        receipt=self.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                          '--cwd',str(project),'--bound','full','--network','--background','--prompt',prompt])
        if 'cli_error' in receipt: raise Blocked('local fixture turn refused')
        result=self.via(['wait',receipt['turn'],'--timeout-ms','180000'])
        if result['state']!='completed': raise Blocked('local fixture turn did not complete')
        return receipt,result

    def _repository_sentinel(self):
        """First model-capable phase uses only loopback controls with nested Git sentinels."""
        outer=safety.private_directory(self.evidence/'ancestor-fixture')
        safety.init_fixture_repo(outer,self.home,sentinel=True)
        nested=outer/'project-boundary'
        safety.init_fixture_repo(nested,self.home)
        control=safety.private_directory(outer/'walk-up-control')
        boundary_results=[]
        for name,project in (('project-boundary',nested),('ancestor-control',control),('namespace-boundary',self.namespace)):
            safety.private_directory(project/'.opencode')
            config=self._materialize_fixture(name,project,{'provider':'mock'})
            (project/'opencode.json').write_text(json.dumps(config)); (project/'opencode.json').chmod(0o600)
            self.fixtures[name]={'path':project,'config':config}
            self._refresh_inventory([project/'.opencode'])
            self._mock_turn(project,'Read the project instructions and reply READY.')
            provider=self.mock_providers[name]
            receipt=provider.receipt()
            for key in ('local_marker','ancestor_agents','ancestor_skills'):
                if type(receipt.get(key)) is not bool: raise Blocked('walk-up marker evidence unavailable')
            boundary_results.append(receipt)
        project,control,namespace=boundary_results
        if not control['ancestor_agents'] or not control['ancestor_skills']:
            raise Blocked('ancestor sentinel control not sensitive; outside-repo fallback needs owner')
        for result in (project,namespace):
            if not result['local_marker'] or result['ancestor_agents'] or result['ancestor_skills']:
                raise Blocked('vendor repository walk-up crossed private Git boundary')
        models=[]
        for result in boundary_results:
            observed=result.get('models')
            if type(observed) is not list or not observed or any(value!='fixture-free' for value in observed):
                raise Blocked('auxiliary mock model receipts unobservable')
            models.extend(safety.MOCK_IDENTITY for value in observed)
        self.admit_automatic_models(models,mock_received=all(row['received'] for row in boundary_results),
                                    configuration_immutable=bool(self._last_auxiliary_bindings))
        return {'project_boundary':True,'namespace_boundary':True,'ancestor_control_sensitive':True,
                'ancestor_skills_absent':True,'ancestor_agents_absent':True,'private_git_commit':True}

    def _foreign_observation(self,session,args,held=False):
        if not hasattr(self,'_foreign'): raise Blocked('foreign input not injected')
        sid=self._vendor_sid(session); events=self.observe('native_events',sid)['events']
        if not held:
            deadline=time.monotonic()+30
            while True:
                delivery=[row['seq'] for row in events if row['type']=='session.inbox.delivered'
                          and row.get('data',row).get('inboxID',row.get('data',row).get('id'))==self._foreign['id']]
                finished=[row for row in events if delivery and row['seq']>delivery[0]
                          and row['type'] in {'session.execution.succeeded','session.execution.failed','session.execution.interrupted'}]
                if finished: break
                if time.monotonic()>=deadline: raise Blocked('foreign execution terminal unobservable')
                time.sleep(.01); events=self.observe('native_events',sid)['events']
        delivered=[row.get('data',row) for row in events if row['type']=='session.inbox.delivered']
        foreign=[row for row in delivered if row.get('inboxID',row.get('id'))==self._foreign['id']]
        if not self.last_request: raise Blocked('owned foreign-control receipt absent')
        number=args.get('turn_number')
        if type(number) is not int: raise Blocked('foreign control pending turn unavailable')
        expected=input_id(session,number)
        own=[row for row in delivered if row.get('inboxID',row.get('id'))==expected]
        cancelled=[row.get('data',row) for row in events if row['type']=='session.inbox.cancelled']
        if not foreign: raise Blocked('foreign delivery correlation incomplete')
        raw_delivery=[row for row in events if row['type']=='session.inbox.delivered']
        foreignseq=next(row['seq'] for row in raw_delivery if row.get('data',row).get('inboxID',row.get('data',row).get('id'))==self._foreign['id'])
        foreign_terminal=[row for row in events if row['type'] in {'session.execution.succeeded','session.execution.failed','session.execution.interrupted'}
                          and foreignseq<row['seq']]
        enqueued=[row.get('data',row) for row in events if row['type']=='session.inbox.enqueued']
        absent=not own and not any(row.get('inboxID',row.get('id'))==expected for row in enqueued)
        c1=read_pages(lambda after:self.via(['events',session,'--after',str(after)]))
        current=[row for row in c1 if row.get('turn')==number]
        terminal=[row for row in current if row['type'] in {'turn.completed','turn.failed','turn.cancelled','turn.unknown'}]
        accepted=any(row['type']=='turn.accepted' for row in current)
        result={'owned_not_submitted':absent,
                'foreign_not_credited':not any(row['type']=='turn.completed' for row in terminal),
                'foreign_not_cancelled':not any(row.get('inboxID',row.get('id'))==self._foreign['id'] for row in cancelled)
                    and (held or bool(foreign_terminal))
                    and not any(row['type']=='session.execution.interrupted' for row in foreign_terminal)}
        if held:
            status=self.via(['status',session]); active=status.get('active_turn')
            pending=type(active) is dict and active.get('n')==number and active.get('phase')=='submitting'
            result.update(foreign_active=not foreign_terminal,owned_not_terminal=not terminal,
                          successor_fenced=absent and not terminal and pending and not accepted)
        elif args.get('expect_owned_absent') is not True:
            raise Blocked('foreign post-control requires explicit absent-owned proof')
        return result

    def _missing_vendor_session(self,session):
        sid=self._vendor_sid(session)
        if not self.provider_endpoints: raise Blocked('missing-ID control requires disposable mock namespace')
        before=self.vendor('GET','/api/session')
        removed=self.vendor('DELETE','/api/session/'+sid)
        if removed['status'] not in {200,204}: raise Blocked('owned vendor session delete refused')
        self._idle_retired=False
        self._operation('idle_retirement',{'session':session})
        reply=self.via(['resume',session,'--prompt','VIA missing-ID control'])
        if 'cli_error' in reply: raise Blocked('missing-ID resume not receipted')
        envelope=self.via(['wait',reply['turn'],'--timeout-ms','180000'])
        if envelope['failure'] is None or envelope['failure']['class']!='resume_mismatch':
            raise Blocked('missing-ID resume_mismatch absent')
        after=self.vendor('GET','/api/session')
        if type(before['body'].get('data')) is not list or type(after['body'].get('data')) is not list:
            raise Blocked('session create absence cannot be checked')
        beforeids={row['id'] for row in before['body']['data']}
        afterids={row['id'] for row in after['body']['data']}
        return {'code':'resume_mismatch','creates':len(afterids-beforeids)}

    def _lsp_probe(self):
        """One configured read and owned readiness barrier, never silence alone (§13 L5/L14)."""
        if getattr(self,'_lsp_result',None) is not None:
            return dict(self._lsp_result)
        if getattr(self,'_lsp_probe_started',False):
            raise Blocked('single LSP configured-read probe was incomplete')
        self._lsp_probe_started=True
        previous_project,previous_request=self.project,self.last_request
        previous_endpoints,previous_model=dict(self.provider_endpoints),self.last_model
        receipt=None; prepared=False
        try:
            project=self.fixture('lsp-probe',{'provider':'mock','fake_lsp':True,'helper':'tool'})
            self.start('release',project)
            prepared=True
            receipt=self.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                             '--cwd',str(project),'--bound','full','--network','--background',
                             '--prompt','Read fixture.via_ocl_fixture with the offered read tool, '
                                        'then hold the private tool helper.'])
            if 'cli_error' in receipt: raise Blocked('configured LSP probe refused')
            self.ensure_vendor()
            pin=self._operation('helper_barrier',{'helper':'tool'})
            if pin.get('owned') is not True or pin.get('started') is not True:
                raise Blocked('LSP readiness probe lacks an owned live pin')
            schema=self._served_schema()
            properties=schema.get('components',{}).get('schemas',{}).get('Config.InfoEncoded',{}).get('properties',{})
            if 'lsp' not in properties: raise Blocked('LSP configuration readback schema unavailable')
            query=urllib.parse.urlencode({'location[directory]':str(project)})
            reply=self.vendor('GET','/api/config?'+query)
            entries=reply.get('body',{}).get('data')
            if reply.get('status')!=200 or type(entries) is not list:
                raise Blocked('LSP effective configuration sources unavailable')
            expected=self.fixtures['lsp-probe']['config']['lsp']['via-fixture']
            effective=None
            for entry in entries:
                if type(entry) is not dict: raise Blocked('LSP configuration entry shape unavailable')
                info=entry.get('info')
                if entry.get('type')=='document' and type(info) is dict and 'lsp' in info:
                    effective=info['lsp']
            if type(effective) is not dict or effective.get('via-fixture')!=expected:
                raise Blocked('configured fake LSP command was not read back')
            provider=self.mock_providers['lsp-probe']; mock=provider.receipt()
            if mock.get('received') is not True or mock.get('model_matches') is not True:
                raise Blocked('LSP probe mock request receipt unavailable')
            calls=mock.get('read_calls')
            if type(calls) is not list or not calls or any(row.get('schema_checked') is not True for row in calls):
                raise Blocked('actual offered read schema unavailable for LSP probe')
            sid=self._vendor_sid(receipt['session_id'])
            events=self.observe('native_events',sid)['events']
            called={row.get('data',{}).get('id') for row in events
                    if row.get('type')=='session.tool.called' and row.get('data',{}).get('name')=='read'}
            succeeded={row.get('data',{}).get('id') for row in events if row.get('type')=='session.tool.success'}
            if not any(row.get('id') in called & succeeded for row in calls):
                raise Blocked('configured fixture file read was not actually completed')
            beginning=time.monotonic(); deadline=beginning+5
            if self.phase_deadline is not None and deadline>self.phase_deadline:
                raise Blocked('phase has insufficient LSP readiness budget')
            spawned=False; marker=self._helper_folder(project)/'lsp.json'
            while True:
                if self.signals: self.signals.guard()
                self.proc.verify(self.vendor_identity)
                if marker.exists():
                    helpers=self.observe('helper_observations')['helpers']
                    spawned=any(row.get('kind')=='lsp' and row.get('ready') is True for row in helpers)
                    if not spawned: raise Blocked('LSP readiness identity unverifiable')
                    break
                if time.monotonic()>=deadline: break
                time.sleep(min(.02,max(0,deadline-time.monotonic())))
            if self.inventory: self.inventory.check()
            result={'complete':True,'spawned':spawned,'config_checked':True,'mock_received':True,
                    'read_attempted':True,'readiness_checked':True,'pin_owned':True,
                    'readiness_seconds':5.0 if not spawned else time.monotonic()-beginning,
                    'readiness_elapsed_seconds':time.monotonic()-beginning,
                    'disposition':'offered' if spawned else 'lsp: not offered by pinned 2.0.22 (read trigger; 5 s readiness window)'}
        finally:
            try:
                if prepared:
                    self._operation('helper_release',{'helper':'tool'})
                    if receipt is not None and 'turn' in receipt:
                        self.via(['wait',receipt['turn'],'--timeout-ms','180000'])
            finally:
                if previous_project is not None:
                    self.project=previous_project
                    self.last_request=previous_request
                    self.provider_endpoints=previous_endpoints
                    self.last_model=previous_model
        self._lsp_result=result
        self._record('lsp-probe',result)
        return dict(result)

    def _lifecycle_capabilities(self):
        """Offered samples require served operations and observed helper recipes (§13 L14)."""
        self.ensure_vendor(); schema=self._served_schema(); paths=schema['paths']
        probe=self._lsp_probe()
        self._lifecycle_schema=schema
        def operation(path,verb):
            return type(paths.get(path,{}).get(verb)) is dict
        core=operation('/api/session','post') and operation('/api/session/{sessionID}/prompt','post') \
            and operation('/api/event','get') and probe.get('pin_owned') is True \
            and probe.get('mock_received') is True
        def shell(path):
            row=paths.get(path,{}).get('post')
            if type(row) is not dict: return False
            body=row.get('requestBody',{}).get('content',{}).get('application/json',{}).get('schema')
            try: self._schema_body(schema,body,{'command':'/usr/bin/true'})
            except Blocked: return False
            return core
        properties=schema.get('components',{}).get('schemas',{}).get('Config.InfoEncoded',{}).get('properties',{})
        helpers={row.get('kind') for row in self.helper_ledger.values() if row.get('ready') is True}
        mcp=core and 'mcp' in properties and operation('/api/mcp','get') and 'mcp' in helpers
        plugin=core and 'plugins' in properties and 'plugin' in helpers
        lsp=core and probe.get('spawned') is True
        long_run=getattr(self,'_long_run_sample',{})
        points={'publication':core,'tool_during':core,'tool_after':core,
                'location_shell_during':shell('/api/shell'),'location_shell_after':shell('/api/shell'),
                'session_shell_during':shell('/api/session/{sessionID}/shell'),
                'session_shell_after':shell('/api/session/{sessionID}/shell'),
                'mcp_during':mcp,'mcp_after':mcp,'plugin_during':plugin,'plugin_after':plugin,
                'lsp_during':lsp,'lsp_after':lsp,
                'long_run':core and long_run.get('completed_tail') is True}
        reload=paths.get('/api/location/reload',{}).get('post')
        points['reload']=core and type(reload) is dict and 'requestBody' not in reload
        disposal=any('dispose' in path for path in paths)
        points['disposal']=False  # No reviewed live-pin operation recipe is implemented.
        notes={'reload':('reload: not offered by pinned 2.0.22 (served schema)' if type(reload) is not dict
                         else 'reload: deferred; offered schema has no reviewed held-pin recipe'),
               'disposal':('disposal: deferred; offered operation has no reviewed held-pin recipe' if disposal
                           else 'disposal: not offered by pinned 2.0.22 (served schema)')}
        return {'source_schema_proven':True,'points':points,'lsp_probe':probe,'notes':notes,
                'optional_offered':{'reload':type(reload) is dict,'disposal':disposal},
                'schema_sha256':'540fdf565da27de9df69b6c3864582344e74ac4ffa225c283b289481d215d241'}

    def _bounded_probe(self,item,limit):
        if type(limit) is not int or not 0<limit<=12: raise Blocked('probe attempt cap invalid')
        rows=[]
        if item=='error_shapes':
            for mode in ('rate_limit','quota','context'):
                if len(rows)>=limit: break
                name='shape-'+mode
                project=self.fixture(name,{'provider':'mock','response':mode})
                self.start('release',project)
                receipt=self.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                                  '--cwd',str(project),'--bound','full','--network','--background','--prompt','VIA shape'])
                envelope=self.via(['wait',receipt['turn'],'--timeout-ms','180000'])
                native=self.observe('native_events',envelope['vendor_session_id'])['events']
                found=[row for row in native if row['type']=='session.step.failed']
                for event in found:
                    error=event.get('data',event).get('error')
                    if type(error) is dict and type(error.get('status')) is int:
                        rows.append({'native_type':event['type'],'status':error['status'],'disposition':'recorded_mock'})
            return {'attempts':min(3,limit),'observations':rows}
        # Collision/forms/superseded attempts are native observations from bounded prior turns;
        # never generate network flooding or claim an unobserved triggering condition.
        self.ensure_vendor()
        events=self.observe('native_events')['events']
        selected=[row for row in events if item=='forms' and row['type']=='form.created'
                  or item=='collision' and row.get('data',row).get('status')==409
                  or item=='other' and any(word in row['type'] for word in ('superseded','inactivity','autoupdate'))]
        for event in selected[:limit]:
            data=event.get('data',event)
            status=data.get('status')
            if type(status) is not int: continue
            rows.append({'native_type':event['type'],'status':status,'disposition':'recorded'})
        return {'attempts':1,'observations':rows}

    def _credential_refusal(self):
        self.start('release',self.namespace)
        before=self._anchor_rows()
        # Metadata acquisition alone exercises the handshake credential fence; no prompt.
        result=self.via(['models','--harness','opencode'])
        if 'cli_error' in result:
            error=result['cli_error']['error']; data=error.get('data')
            code=data.get('kind2') if type(data) is dict else None
        else:
            rows=result.get('models')
            if type(rows) is not list: raise Blocked('credential refusal result missing')
            refused=[row for row in rows if 'unexpected_credential_state' in json.dumps(row)]
            if not refused: raise Blocked('credential refusal not observable')
            code='unexpected_credential_state'
        if code!='unexpected_credential_state': raise Blocked('known credentials not refused')
        # A second metadata acquisition must make a fresh attempt (transient refusal, not cache).
        after=self._anchor_rows(); self.via(['models','--harness','opencode']); twice=self._anchor_rows()
        if len(twice)<=len(after) or len(after)<=len(before): raise Blocked('credential refusal caching proof unavailable')
        return {'code':code,'cached':False,'session_creates':0,'prompt_submits':0,'credential_gets':0}

    def _near_limit_inbox(self,args):
        """Park bounded synthetic queue data against a real VIA receipt/claim (§9 OC05)."""
        if args['count']!=2 or args['prompt_bytes']!=1024*1024-8192:
            raise Blocked('near-limit fixture outside reviewed admission bound')
        # Coordinator-approved recipe: a held real VIA turn assigns the owned
        # ID; direct seeding is explicitly synthetic and never attributed to
        # VIA HTTP submission. E49/E53 queue+interrupt parks and persists inputs.
        self.stop()  # Fresh controller counters; the preceding L3 used occurrence1.
        project=self.fixture('near-limit-park',{'provider':'mock','helper':'queue','script_after_setup':True})
        self.start('failpoints',project)
        setup,result=self._mock_turn(project,'Reply SETUP.')
        session=setup['session_id']; sid=result['vendor_session_id']; number=result['turn']+1
        point='adapters.opencode.prompt_after_eligibility'
        target=self.observe('seam_target',{'point':point,'session':session,
                            'vendor_session':sid,'turn_number':number})
        self.seam(point,1,'pause',target=target)
        cwd_bytes=len(json.dumps(str(project.resolve()),ensure_ascii=False).encode())
        actual_prompt_bytes=args['prompt_bytes']-cwd_bytes-2
        if actual_prompt_bytes<=0: raise Blocked('near-limit cwd exhausts prompt admission')
        text='x'*actual_prompt_bytes
        if len(json.dumps(text).encode())+cwd_bytes>args['prompt_bytes']:
            raise Blocked('near-limit prompt exceeds JSON-plus-cwd admission')
        pending=self.via(['resume',session,'--prompt',text]); self.seam_wait(point)
        expected=input_id(session,number)
        if pending['turn']!=session+'/'+str(number): raise Blocked('near-limit owned receipt differs')
        claim=self.via(['status',session]).get('active_turn')
        if type(claim) is not dict or claim.get('n')!=number or claim.get('phase')!='submitting':
            raise Blocked('synthetic owned seed lacks a real VIA claim')
        running='msg_'+secrets.token_hex(12)
        accepted=self.vendor('POST','/api/session/'+sid+'/prompt',
                             {'id':running,'text':'Run the queue helper and hold.'})
        self._prompt_ack(accepted,running,sid)
        barrier=self._operation('helper_barrier',{'helper':'queue'})
        if barrier.get('owned') is not True: raise Blocked('queue park running helper unowned')
        foreign='msg_'+secrets.token_hex(12)
        schema=self._served_schema()
        body_schema=schema['paths']['/api/session/{sessionID}/prompt']['post']['requestBody']['content']['application/json']['schema']
        for value in (expected,foreign):
            body={'id':value,'text':text,'delivery':'queue'}
            self._schema_body(schema,body_schema,body)
            self._prompt_ack(self.vendor('POST','/api/session/'+sid+'/prompt',body),value,sid)
        interrupted=self.vendor('POST','/api/session/'+sid+'/interrupt')
        if interrupted['status']==200:
            _typed(interrupted['body'],{'interrupted':B},'queue interrupt acknowledgement')
        elif interrupted['status']!=204: raise Blocked('queue parking interrupt refused')
        listing=self.vendor('GET','/api/session/'+sid+'/inbox')
        data=listing['body'].get('data') if type(listing['body']) is dict else None
        if type(data) is not list or {row.get('id') for row in data}!={expected,foreign}:
            raise Blocked('two bounded parked leftovers not observed')
        self._record('near-limit-seed',{'source':'synthetic queue seeding against real VIA receipt',
                     'owned_claim_verified':True,'count':len(data),'prompt_bytes':actual_prompt_bytes,
                     'cwd_json_bytes':cwd_bytes})
        crashed=self.daemon; self.proc.verify(crashed); self.journal._signal(crashed,signal.SIGKILL)
        deadline=time.monotonic()+30
        while time.monotonic()<deadline and not self.proc.gone(crashed): time.sleep(.01)
        if not self.proc.gone(crashed): raise Blocked('owned VIA crash absence unverifiable')
        self.daemon=None; self._armed={}; self._armed_history=set(); self._seam_targets={}
        # No graceful cancel may delete the queued claim before reopen cleanup.
        self.stop(); self.start('failpoints',project); self.ensure_vendor()
        durable=self.vendor('GET','/api/session/'+sid+'/inbox')
        items=durable['body'].get('data')
        if type(items) is not list or {row.get('id') for row in items}!={expected,foreign}:
            raise Blocked('parked near-limit queue durability unobserved')
        recovered=self.via(['wait',pending['turn'],'--timeout-ms','180000'])
        if recovered['state']!='unknown': raise Blocked('crashed owned turn not recovered unknown')
        successor=self.via(['resume',session,'--prompt','VIA SUCCESSOR'])
        envelope=self.via(['wait',successor['turn'],'--timeout-ms','180000'])
        events=self.observe('native_events',sid)['events']
        cancelled=[row.get('data',row).get('inboxID',row.get('data',row).get('id'))
                   for row in events if row['type']=='session.inbox.cancelled']
        return {'successor_completed':envelope['state']=='completed',
                'owned_deletes':cancelled.count(expected),'foreign_deletes':cancelled.count(foreign),
                'seed_source':'synthetic queue; actual VIA claim'}

    @staticmethod
    def _prompt_ack(reply,inputid,session):
        """Observed §8 acknowledgement, or doc-only204 needing later native proof."""
        if reply['status']==200:
            _typed(reply['body'],{'data':{'id':S,'sessionID':S}},'prompt acknowledgement')
            if reply['body']['data']['id']!=inputid or reply['body']['data']['sessionID']!=session:
                raise Blocked('prompt acknowledgement identity differs')
        elif reply['status']!=204: raise Blocked('controlled prompt refused')

    def _observe_accepted_input(self,sid):
        events=self.observe('native_events',sid)['events']
        accepted=[row for row in events if row['type']=='session.inbox.enqueued']
        if not accepted: raise Blocked('VIA owned accepted input not observed')
        data=accepted[-1].get('data',accepted[-1]); value=data.get('inboxID',data.get('id'))
        if type(value) is not str: raise Blocked('accepted input ID absent')
        return value

    def set_phase_budget(self,public_turns,mock_turns):
        """One phase's counters cover every driver-submitted model-capable operation."""
        if type(public_turns) is not int or type(mock_turns) is not int or min(public_turns,mock_turns)<0:
            raise Blocked('invalid phase admission ceilings')
        self.phase_public_left=public_turns; self.phase_mock_left=mock_turns
        self._phase_public_used=0

    def _admit_model(self,identity):
        if self.signals: self.signals.guard()
        if self.guard.stopped: raise Blocked('spending control previously failed')
        if self.phase_deadline and time.monotonic()>=self.phase_deadline:
            raise Blocked('phase model admission deadline')
        overridden=identity.split('/',1)[0] in self.provider_endpoints
        field='phase_mock_left' if overridden else 'phase_public_left'
        remaining=getattr(self,field)
        if remaining is None: raise Blocked('phase admission ceilings not configured')
        if remaining<=0: raise Blocked('phase model admission ceiling exhausted')
        if not overridden:
            setattr(self,field,remaining-1); self._phase_public_used+=1

    def _mock_request_admit(self,model):
        """Count every local provider request, including title/child/compaction calls."""
        if model!='fixture-free':
            self.guard.stopped=True; raise Blocked('paid or unexpected auxiliary mock identity')
        if self.signals: self.signals.guard()
        if self.phase_deadline and time.monotonic()>=self.phase_deadline:
            self.guard.stopped=True; raise Blocked('mock request after phase deadline')
        if self.guard.stopped or self.phase_mock_left is None or self.phase_mock_left<=0:
            self.guard.stopped=True; raise Blocked('mock model request ceiling exhausted')
        self.phase_mock_left-=1

    def _started_at(self,ticks):
        """Match Host's public leftover timestamp from privately verified start ticks."""
        try:
            raw=(self.proc.root/'stat').read_bytes()
            rows=[line.split()[1] for line in raw.splitlines() if line.startswith(b'btime ')]
            if len(rows)!=1: raise Blocked('boot time ambiguous')
            hz=os.sysconf('SC_CLK_TCK')
            if hz<=0: raise Blocked('clock tick scale unavailable')
            seconds=int(rows[0])+ticks//hz
            return datetime.datetime.fromtimestamp(seconds,datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
        except (OSError,ValueError,OverflowError) as error: raise Blocked('helper birth timestamp unverifiable') from error
