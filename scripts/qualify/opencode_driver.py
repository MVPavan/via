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
import textwrap
import time
import urllib.parse

import opencode_safety as safety
from opencode_safety import OwnedHTTP
from opencode_ownership import OwnershipRegistry
from opencode_catalog import catalog_record
from opencode_reply import ReplyEvidence, reply_check
from opencode_cases import REPOSITORY_SENTINELS, PHASES
from opencode_events import terminal_event
from opencode_c1 import (TERMINAL_STATES, SCOPES, CANCEL_OUTCOMES, CLEANUP_STATES,
                         ERROR_CODES, FAILURE_CLASSES, STOP_REASONS)
from opencode_runroot import ANCHOR_SOCKET_TAIL, SOCKET_BYTES

Blocked = safety.Blocked
# Packet §9 and the reviewed qualification plan's observation/pagination bounds.
OBSERVATION_BYTES = 16 * 1024 * 1024
EVIDENCE_BYTES = 256 * 1024 * 1024
SCAN_ENTRIES = 100000  # §13: bound the owned evidence/private-root walk.
SCAN_ATTEMPTS = 3  # §13: restart the whole scan on transient VIA-owned churn.
PAGE_COUNT = 1000
SSE_HANDSHAKE_SECONDS = 30  # §13 owned observer absolute header deadline.
WAIT_REPLY_MARGIN_SECONDS = 10  # §13: allow the CLI to return its bounded wait reply.
CANCEL_ACK_SECONDS = 10  # C1 §3.5; Core DEFAULT_FORCE_AFTER_MS.
CANCEL_CLEANUP_SECONDS = 60  # C1 §3.5 pending reported-tool cleanup, before settlement.
CLOSE_REPLY_SECONDS = 40  # CLI close: default 10 s close + 30 s terminal read allowance.
FOREGROUND_WAIT_SECONDS = 180  # §13: same ceiling as this runner's explicit wait calls.
HELPER_STOP_SECONDS = 30  # §13 L14 bounded cooperative cleanup before a successor.
IDLE_RETIREMENT_SECONDS = 90  # §13: natural exit of each process, clipped to its phase.
CLEANUP_SECONDS = 180  # §13: bounded mock abort/retries, then natural daemon idle shutdown.
METADATA_EFFORT = 'via-qualification-metadata-only-unoffered'  # Packet §5; C1 §4.
OWNED_READINESS_SECONDS = 30  # §13: bounded delayed facts within the enclosing phase.
# §8/§13 L2: HTTP 30 s + exit classification 3 s + retirement 5 s + fixed 2 s margin.
WRITE_DRAIN_SECONDS = 40
L9_FINISH_SECONDS = OWNED_READINESS_SECONDS  # §13 L9: snapshot before the phase watchdog.
LONG_RUN_SHELL_TIMEOUT_MS = 1800 * 1000  # §13 L9/L14: match the bounded thirty-minute helper.
PUBLIC_HELPER_READINESS_SECONDS = 120  # §13: free-model latency before helper readiness.
PUBLIC_TIMELINES = sum(phase.public_turns for phase in PHASES)  # §13 per-phase admissions.
CONFIG_READY_POLL_SECONDS = .2  # §13: only empty or verified previous endpoint maps wait.
CATALOG_READY_POLL_SECONDS = .2  # §§2.2, 13: catalogue readiness, never request admission.
BOOTSTRAP_SECONDS = 30  # §13: owned acquisition and all served-fact checks.
BOOTSTRAP_WAIT_MS = 30000  # §13: mock completion after the response is released.
QUALIFICATION_FREE_FLOOR_BYTES = 1024 * 1024 * 1024  # §13 run root; C1 §3.14 disk guard.


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
    'envelope': {'session_id': S, 'turn': I, 'revision': I, 'state': S, 'failure': (dict,type(None)),
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
    # Store-derived close replays may omit leftovers; no verdict reads that member.
    'close': {'session_id':S,'state':S,'cancelled_turns':[S],'cleanup':S},
    'events page': {'events': [{'seq': I,'type': S}], 'more': B,'next_after': I},
    'status': {'inherit': {key:S for key in ('agents','hooks','instruction_files',
                                          'mcp_servers','plugins','skills')},
               'warnings': [{'code':S}], 'vendor_identity_verified': B,
               'progress': (dict,type(None)), 'turns': [{'state':S}]},
    'logs': {'transcript': S+N},
    'error': {'code': I,'message': S,'data':{'kind':S}},
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
    if schema=='error' and value['data']['kind'] in ERROR_CODES \
            and value['code']!=ERROR_CODES[value['data']['kind']]:
        raise Blocked('C1 error code and kind differ')
    if schema=='close' and (value['state']!='closed' or value['cleanup'] not in {'quiescent','uncertain'}):
        raise Blocked('close state or cleanup invalid')
    if schema=='cancel reply' and value['state'] not in TERMINAL_STATES|{'running'}:
        raise Blocked('cancel state invalid')
    if schema in {'envelope','cancel reply'} and value['cancel'] is not None:
        _typed(value['cancel'],{'outcome':S,'cleanup':S},'cancel')
        if value['cancel']['outcome'] not in CANCEL_OUTCOMES or value['cancel']['cleanup'] not in CLEANUP_STATES:
            raise Blocked('cancel outcome or cleanup invalid')
    if schema=='envelope':
        if value['state'] not in TERMINAL_STATES:
            raise Blocked('envelope terminal state invalid')
        if value['revision']<0:raise Blocked('negative envelope revision')
        if value['failure'] is not None:
            _typed(value['failure'],{'class':S,'message':S},'failure')
            if value['failure']['class'] not in FAILURE_CLASSES:
                raise Blocked('envelope failure class invalid')
        if value['stop_reason'] is not None and value['stop_reason'] not in STOP_REASONS:
            raise Blocked('envelope stop reason invalid')
        if value['cancel'] is not None:
            _typed(value['cancel'],{'outcome':S,'cleanup':S},'cancel')
        if value['cost']['scope'] not in SCOPES:
            raise Blocked('cost scope invalid')
        if value['usage']['scope'] not in SCOPES:
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


def bounded_command(argv, *, env, cwd=None, input=None, timeout=30, observe=None):
    """Collect owned CLI pipes only in memory with an absolute time/byte bound."""
    try:
        process=subprocess.Popen(argv,env=env,cwd=cwd,stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    except OSError as error:
        raise Blocked('private command could not start') from error
    selector=selectors.DefaultSelector()
    output={}
    complete=False
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
        rc=process.wait(timeout=max(0.01,deadline-time.monotonic()))
        complete=True
        return rc,bytes(output['out']),bytes(output['err'])
    except BaseException:
        # This process is the CLI we started, never a vendor/daemon discovered by name.
        if process.poll() is None:
            process.kill()
        process.wait(timeout=5)
        raise
    finally:
        selector.close()
        if observe is not None:
            observe(process.returncode,bytes(output.get('out',b'')),bytes(output.get('err',b'')),
                    complete=complete)
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


class _ScanVanished(Exception):
    """A VIA disappearance requires a full bounded rescan (packet §13)."""


class Driver:
    """Owned private driver for the reviewed phases; public secrets never persisted."""

    def __init__(self,release=None,failpoints=None,pinned=None,evidence=None,*,
                 via_release=None,via_failpoints=None,opencode=None,signals=None,
                 proc=None,execute=None,initialize=False,public_free=False,mock_origins=(),rg=None,ownership=None,git_templates=None,run_root=None,diagnostic_mock_only=False):
        release=release if release is not None else via_release
        failpoints=failpoints if failpoints is not None else via_failpoints
        pinned=pinned if pinned is not None else opencode
        if None in (release,failpoints,pinned,evidence):
            raise Blocked('explicit VIA builds, pinned program and evidence required')
        self.signals=signals
        self.paths={'release':Path(release).resolve(),'failpoints':Path(failpoints).resolve()}
        self.pinned=Path(pinned).resolve()
        self.evidence=Path(evidence).resolve()
        self.ownership=ownership or OwnershipRegistry(self.evidence)
        self.evidence.mkdir(parents=True,mode=0o700,exist_ok=True)
        if stat.S_IMODE(self.evidence.stat().st_mode)!=0o700:
            raise Blocked('evidence root is not private')
        self.proc=proc or safety.ProcReader()
        self.execute=execute or bounded_command
        self.vault=safety.PasswordVault()
        self.guard=safety.SpendingGuard(public_free=public_free,owned_mock_origins=mock_origins,
                                        mock_only=diagnostic_mock_only)
        self.mock_origins=set(mock_origins)
        self.rg=rg
        self.public_free=public_free
        self.diagnostic_mock_only=diagnostic_mock_only
        self._diagnostic_sessions={};self._submitted_models={}
        self._mock_provider_left={}
        self._http=None; self._binary=None; self.daemon=None; self.anchor=None; self.vendor_identity=None
        self.identities=set(); self.uncertain=[]; self.build_hashes={}; self.handles={}
        self.bearer_forms=set()
        self.events=[]; self.events_error=None; self._event_failure=None; self._event_stop=threading.Event()
        self._event_thread=None; self._event_conn=None; self._events_lock=threading.Lock()
        self.counter=0; self.owned_replies=[]; self.secret_forms=[]; self._armed={}
        self._record_lock=threading.RLock()
        self.project=None; self.fixtures={}; self.provider_endpoints={}; self.catalog_cache={}
        self.automatic_models_proven=False
        self._catalog_evidence=None
        self.phase_deadline=None; self.phase_kind=None; self.stop_result=None
        self.work=Path(run_root).resolve() if run_root is not None else self.evidence
        self.external_root=run_root is not None
        if self.external_root: self.ownership.register(self.work,'helper',recursive=False)
        self.state=self.work/'state'; self.runtime=self.work/'runtime'
        self.helpers=self.work/'helpers'; self.home=self.work/'home'
        self.ownership.register_state(self.state)
        self.ownership.register(self.home,'vendor-private')
        self.ownership.register(self.helpers,'helper')
        self.ownership.register(self.runtime,'via-owned',runtime=True)
        if self.pinned.is_relative_to(self.evidence):
            self.ownership.register(self.pinned,'helper',directory=False)
        self.env={}; self.namespace=None; self.namespace_env={}; self.inventory=None
        self.journal=safety.StopJournal(self.evidence.parent/'stopped-daemon.json',self.proc)
        self.mock_providers={}; self.last_model=None; self.last_request=None; self.last_cancel=None
        self._event_generation=None; self._sigkill_at=None; self._last_rotation=None
        self._event_socket=None; self._event_watchdog=None
        self._idle_retired=False; self._last_lifecycle=None; self.inherit_settings={}
        self.phase_public_left=None; self.phase_mock_left=None; self.helper_ledger={}
        self._cancel_diagnostic_session=None
        self._phase_public_used=0
        self._timeline_lock=threading.RLock(); self._timelines=[]
        self.helper_channels={}; self.helper_generations={}; self.lifecycle_counter=0
        self.helper_origins={}; self.server_identities=set()
        self.fake_gate_manifest=None
        self._armed_history=set()
        self._seam_targets={}
        self._metadata_command=None; self._metadata_active=False
        self._bootstrap_command=None; self._bootstrap_session=None; self._bootstrap_active=False
        self._bootstrap_config=None
        self._bootstrap_cleanup=False; self._bootstrap_count=0
        self._cleanup_active=False
        self._bootstrap_deadline=None
        self._observation_deadline=None
        self._verified_host_record=None; self._killed_anchor=None
        self._namespace_bootstraps={}
        self._verified_endpoint_maps={}; self._endpoint_reloads={}
        self._daemon_log_mark=None; self.daemon_generations=[]
        self._daemon_start_status=False
        self.reply_evidence=ReplyEvidence(self._record,self._reply_protected,self._reply_context,
            vault=self.vault,secret_forms=lambda:(*self.secret_forms,*self.bearer_forms))
        self.git_templates=safety.GitTemplateCopies(
            self.work,git_templates or {},lambda value:self._record('git-template-copy',value))
        self.embedded_runtime=safety.EmbeddedRuntime(
            self.pinned,self.work,lambda value:self._record('embedded-runtime',value),
            deadline=lambda:float('inf') if self._cleanup_active else min(self.phase_deadline or float('inf'),
                self._bootstrap_deadline if self._bootstrap_active and self._bootstrap_deadline
                is not None else float('inf'),self._observation_deadline or float('inf')))
        self._publication_entered=threading.Event(); self._publication_release=threading.Event()
        if initialize:
            self.prepare()

    def _record(self,label,value):
        with self._record_lock:
            return self._write_record(label,value)

    def _write_record(self,label,value):
        raw=json.dumps(value,allow_nan=False).encode()
        if len(raw)>OBSERVATION_BYTES or any(form and form in raw for form in self.secret_forms):
            raise Blocked('evidence secrecy or size control failed')
        used=sum(path.stat().st_size for path in self.evidence.glob('*.json'))
        if used+len(raw)>EVIDENCE_BYTES:
            raise Blocked('run evidence bound')
        self.counter+=1
        path=self.ownership.register(self.evidence/f'{self.counter:05d}-{label}.json',
                                     'runner-evidence',directory=False)
        self.vault.safe_write_json(path,value)
        return path.name

    def _timeline_submit(self,receipt,model,submitted_at,generation):
        """§13: begin content-free timing at an admitted public CLI submission."""
        if model!=safety.FREE_IDENTITY:return
        session=receipt['session_id'];address=receipt['turn']
        prefix,number=address.rsplit('/',1)
        if prefix!=session or not number.isdecimal() or not 0<int(number)<2**32:
            raise Blocked('public timeline turn identity malformed')
        with self._timeline_lock:
            if len(self._timelines)>=PUBLIC_TIMELINES:raise Blocked('public timeline admission bound')
            self._timelines.append({'session_id':session,'turn':int(number),'sessionID':None,
                'model':model,'elapsed_ms':dict.fromkeys(('first_assistant','first_tool','readiness','terminal')),
                'terminal':None,'readiness_bound_seconds':None,'delivery_observed':False,
                '_submitted':submitted_at,'_generation':generation,'_closed':False})

    def _timeline_bind(self,session,sid,*,turn=None):
        """§13: correlate buffered observations without mixing daemon/vendor generations."""
        with self._events_lock:events=list(self.events)
        with self._timeline_lock:
            for row in self._timelines:
                if row['session_id']==session and row['_generation']==self.vendor_identity \
                        and (turn is None or row['turn']==turn):
                    if row['sessionID'] not in (None,sid):raise Blocked('public timeline session changed')
                    row['sessionID']=sid
        for event in events:self._timeline_event(event,self.vendor_identity)

    def _timeline_event(self,event,generation):
        """§13/E8: timestamp first native assistant/tool creation inside the owned input interval."""
        at=event.get('_observed_at');data=event.get('data',event)
        if type(at) not in {int,float} or not math.isfinite(at) or type(data) is not dict:return
        kind=event.get('type');sid=self._event_session(event)
        with self._timeline_lock:
            for row in self._timelines:
                if row['_generation']!=generation or row['sessionID']!=sid or sid is None \
                        or at<row['_submitted'] or row['_closed']:continue
                if kind=='session.inbox.delivered':
                    owned=data.get('inboxID',data.get('id'))==input_id(row['session_id'],row['turn'])
                    if owned:row['delivery_observed']=True
                    elif row['delivery_observed']:row['_closed']=True
                    continue
                if not row['delivery_observed']:continue
                milestone=None
                if kind=='session.step.started' and type(data.get('assistantMessageID')) is str:
                    milestone='first_assistant'
                elif kind=='session.tool.input.started' and type(data.get('assistantMessageID')) is str:
                    milestone='first_tool'
                elif kind in {'session.execution.succeeded','session.execution.failed','session.execution.interrupted'}:
                    milestone='terminal';row['terminal']=kind.rsplit('.',1)[1];row['_closed']=True
                if milestone and row['elapsed_ms'][milestone] is None:
                    row['elapsed_ms'][milestone]=int((at-row['_submitted'])*1000)

    def _timeline_ready(self,session,seconds,*,observed=True):
        """§13: mark only verified helper readiness for the current admitted public turn."""
        with self._timeline_lock:
            rows=[row for row in self._timelines if row['session_id']==session
                  and row['_generation']==self.vendor_identity and not row['_closed']]
            if rows:
                row=rows[-1]
                row['readiness_bound_seconds']=seconds
                if observed and row['elapsed_ms']['readiness'] is None:
                    row['elapsed_ms']['readiness']=int(max(0,time.monotonic()-row['_submitted'])*1000)

    def _timeline_flush(self,label):
        """§13: persist closed observed latencies; absent milestones remain unavailable."""
        with self._timeline_lock:
            rows=[{key:(dict(value) if type(value) is dict else value)
                   for key,value in row.items() if not key.startswith('_')} for row in self._timelines]
        if rows:self._record(label,{'submission_basis':'CLI-dispatch',
            'observation_basis':'owned-SSE-receipt-and-verified-helper-readiness','turns':rows})

    def _reply_protected(self,raw):
        return self.vault.leaks(raw) or any(form and form in raw
            for form in (*self.secret_forms,*self.bearer_forms))

    def _reply_context(self):
        project=Path(self.project) if self.project is not None else None
        location=project.relative_to(self.work).as_posix() if project is not None \
            and project.is_relative_to(self.work) else 'unregistered'
        if self._reply_protected(location.encode()): location='redacted'
        return {'location':location}

    def _request(self,method,path,*args,client=None,**kwargs):
        """§13: retain diagnostic fields before any HTTP reply validation can block."""
        client=self._http if client is None else client
        def observe(status,raw,**metadata):
            self.reply_evidence.capture('vendor',self._reply_route(path),raw,http_status=status,**metadata)
        if isinstance(client,OwnedHTTP):
            kwargs['observe']=observe
        with safety.block_context(process_role='vendor'):
            status,raw=client.request(method,path,*args,**kwargs)
        if not isinstance(client,OwnedHTTP): observe(status,raw,complete=True)
        return status,raw

    @staticmethod
    def _reply_route(path):
        """§13: route classes cannot retain IDs, queries or secret-bearing path text."""
        allowed={'api','info','integration','model','config','session','event','openapi.json',
                 'credential','inbox','message','abort','compact','fork','permission','children'}
        return '/'.join(part if part in allowed or part=='' else '<id>'
                        for part in urllib.parse.urlsplit(path).path.split('/'))

    def _directory(self,path,kind='runner-evidence',**flags):
        """Register every created/handoff directory before its contents exist (§13)."""
        return safety.private_directory(self.ownership.register(path,kind,**flags))

    def _create_namespace(self):
        """Register the whole state/vendor tree and each private namespace root (§13)."""
        self.ownership.register_state(self.state)
        namespace,env=safety.create_namespace(self.state)
        self.ownership.register(namespace,'vendor-private')
        for value in env.values(): self.ownership.register(value,'vendor-private')
        # §2.2/§13: these exact TMPDIRs are handed to the private serve/probe recipe.
        probe_root=self._directory(namespace.parent/'probe','vendor-private')
        probe=self._directory(probe_root/'tmp','vendor-private')
        self.git_templates.register_snapshot(Path(env['XDG_DATA_HOME'])/'opencode'/'snapshot')
        self.embedded_runtime.register_tmpdir(env['TMPDIR'],'serve')
        self.embedded_runtime.register_tmpdir(probe,'probe')
        return namespace,env

    def prepare(self):
        """Create exact namespace/private roots and reviewed minimal PATH (N1–N3,N8,N9)."""
        if self.external_root:
            self._copy_programs()
        for directory in (self.home,self.state,self.helpers):
            safety.private_directory(directory)  # Already registered at initial handoff.
        self.namespace,self.namespace_env=self._create_namespace()
        safety.init_fixture_repo(self.namespace,self.home)
        path,rg_proof=safety.build_minimal_path(self.helpers,self.rg)
        if len(os.fsencode(self.runtime))+ANCHOR_SOCKET_TAIL>SOCKET_BYTES:
            if self.external_root: raise Blocked('private run root socket path exceeds bound')
            # Coordinator ruling N9 explicitly authorizes short /tmp/via-* runtime roots.
            self.runtime=Path(tempfile.mkdtemp(prefix='via-ocl.',dir='/tmp'))
            self.ownership.register(self.runtime,'via-owned',runtime=True)
            self._record('runtime',{'short_root':True,'authority':'coordinator N9 socket ruling'})
        else: safety.private_directory(self.runtime)
        self.env={key:str(self._directory(self.work/('daemon-'+part),'via-owned'))
                  for key,part in safety.PRIVATE_PARTS.items()}
        self.env.update(PATH=path,LANG='C.UTF-8',VIA_STATE_DIR=str(self.state),
                        VIA_RUNTIME_DIR=str(self.runtime))
        self._record('rg',rg_proof)
        # Inventory excludes helper rg's reviewed symlink; it covers Bun's HOME cache too.
        self.inventory=safety.Inventory([self.namespace.parent,self.home,
                                        *(Path(value) for value in self.env.values()
                                          if value.startswith(str(self.work/'daemon-')))],
                                        embedded_runtime=self.embedded_runtime,git_templates=self.git_templates,
                                        record=lambda row:self._record('inventory-block',row))
        self.recover_stopped()

    def _copy_programs(self):
        """Execute only private copies bound to the admitted source/build hashes (§13)."""
        import opencode_runroot as roots
        binary_root=self._directory(self.work/'bin','helper')
        safety.verify_binary(self.pinned)
        self.pinned=roots.copy_program(self.pinned,binary_root/'opencode')
        self.paths={kind:roots.copy_program(path,binary_root/('via-'+kind))
                    for kind,path in self.paths.items()}
        binary_root.chmod(0o500)
        safety.verify_binary(self.pinned)
        self.embedded_runtime.pinned=self.pinned

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
        root=self._directory(self.work/'fixtures','vendor-private')
        project=self.ownership.register(root/name,'vendor-private')
        safety.init_fixture_repo(project,self.home)
        opencode=self._directory(project/'.opencode','vendor-private')
        config=dict(config)
        if name=='long-run': self._long_run_started=time.monotonic()
        config=self._materialize_fixture(name,project,config)
        self._validate_provider_config(config)
        self._write_fixture_config(project,config,exclusive=True)
        self.fixtures[name]={'path':project,'config':config}
        self._refresh_inventory([opencode])
        return project

    def _refresh_inventory(self,extra=()):
        roots=[self.namespace,self.home,*[Path(value) for key,value in self.env.items()
                                         if key in safety.PRIVATE_PARTS],*extra]
        roots += [row['path']/'.opencode' for row in self.fixtures.values()]
        roots += [root.path/'vendor' for root in self.ownership.ordered()
                  if root.state and (root.path/'vendor').is_dir()]
        if self.inventory: self.inventory.check()
        self.inventory=safety.Inventory(roots,embedded_runtime=self.embedded_runtime,git_templates=self.git_templates,
                                        record=lambda row:self._record('inventory-block',row))

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
        self._check_ancestors('start')
        project=Path(project)
        if self.daemon is not None: self._refresh_daemon(restart=False)
        if self.daemon is not None and self.phase_kind==kind:
            self._verify(self.daemon)
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
        config={'harnesses':{'opencode':{'binary':str(self.pinned),'inherit':self.inherit_settings}},
                'disk':{'free_floor':QUALIFICATION_FREE_FLOOR_BYTES}}
        safety.verify_daemon_program(config,self.pinned)
        fd=os.open(self.state/'daemon.json',os.O_WRONLY|os.O_CREAT|os.O_TRUNC|os.O_NOFOLLOW,0o600)
        with os.fdopen(fd,'w') as file: json.dump(config,file)
        self.phase_kind=kind; self._binary=self.paths[kind]
        env=dict(self.env)
        env.pop('VIA_FAILPOINT_DIR',None); env.pop('VIA_FAILPOINT_TOKEN',None)
        if kind=='failpoints':
            directory=self._directory(self.work/('failpoints-'+secrets.token_hex(8)))
            self._token=secrets.token_hex(24)
            env.update(VIA_FAILPOINT_DIR=str(directory),VIA_FAILPOINT_TOKEN=self._token)
        elif kind!='release': raise Blocked('invalid VIA build selection')
        self.env=env
        self.provider_endpoints={}
        for row in self.fixtures.values():
            if row['path']==self.project: self.provider_endpoints=self._validate_provider_config(row['config'])
        self._refresh_inventory()
        log_mark=self._daemon_log_checkpoint()
        self.stop_result=None
        try:
            self._daemon_start_status=True
            try: reply=self.via(['daemon','status'])
            finally: self._daemon_start_status=False
            row=self.proc.stat(reply['pid'])
            if row is None: raise safety.process_block('daemon',None,'private daemon PID disappeared')
            identity=safety.Identity(reply['pid'],row['start_ticks'])
            self._verify(identity,'daemon')
            if not self._locks_owned(identity.pid):
                raise Blocked('private daemon lock ownership unavailable')
            self._verify(identity,'daemon')
            self.daemon=identity; self.ownership.daemon_started(self.state); self.identities.add(identity)
            self._daemon_log_mark=log_mark
            self.daemon_generations.append({'generation':len(self.daemon_generations)+1,
                **identity.report(),'build':kind})
            self._record('daemon-generation',{'event':'started',**self.daemon_generations[-1]})
            self._record('daemon-start',{'verified':True,**identity.report(),'build':kind})
        except BaseException:
            self.uncertain.append('failed daemon start')
            self._discover_private_lockers()
            raise

    def _verify(self,identity,role=None):
        """Attach the identity's fixed role without changing process proof (§13)."""
        role=role or self._process_role(identity)
        with safety.block_context(process_role=role):
            self.proc.verify(identity)

    def _process_role(self,identity):
        """Classify saved identities without observing the process again (§13)."""
        daemon=identity==self.daemon or any((row['pid'],row['start_ticks'])==
            (identity.pid,identity.start_ticks) for row in self.daemon_generations)
        return ('daemon' if daemon else 'anchor' if identity==self.anchor
                else 'vendor' if identity==self.vendor_identity or identity in self.server_identities
                else 'helper' if identity in self.helper_ledger else 'host')

    def _daemon_log_checkpoint(self):
        """Bind shutdown proof to bytes written after this generation starts (§13)."""
        try: info=(self.state/'via.log').lstat()
        except FileNotFoundError: return {'device':None,'inode':None,'offset':0}
        except OSError as error: raise Blocked('daemon idle exit log checkpoint unavailable') from error
        if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_nlink!=1:
            raise Blocked('daemon idle exit log checkpoint unsafe')
        return {'device':info.st_dev,'inode':info.st_ino,'offset':info.st_size}

    def _idle_exit_summary(self):
        """Only the current private generation's bounded shutdown record proves exit (§13)."""
        mark=self._daemon_log_mark
        if mark is None: raise Blocked('daemon idle exit generation checkpoint missing')
        records=[]; matched=mark['inode'] is None
        for name in ('via.log.1','via.log'):
            path=self.state/name
            try: fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
            except FileNotFoundError: continue
            except OSError as error: raise Blocked('daemon idle exit log unavailable') from error
            with os.fdopen(fd,'rb') as stream:
                info=os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_nlink!=1:
                    raise Blocked('daemon idle exit log unsafe')
                same=(info.st_dev,info.st_ino)==(mark['device'],mark['inode'])
                if not matched and not same: continue
                if same:
                    if info.st_size<mark['offset']: raise Blocked('daemon idle exit log shrank')
                    stream.seek(mark['offset']); matched=True
                elif name=='via.log.1': continue
                raw=stream.read(OBSERVATION_BYTES+1)
                if len(raw)>OBSERVATION_BYTES: raise Blocked('daemon idle exit log bound')
            for line in raw.splitlines():
                try: value=json.loads(line)
                except (ValueError,UnicodeError): continue
                if type(value) is dict and type(value.get('daemon_shutdown')) is dict:
                    records.append(value['daemon_shutdown'])
        if not matched or len(records)!=1: raise Blocked('daemon idle exit record missing or ambiguous')
        value=records[0]
        required={'mode':'idle','entry':'entered','disposition':'clean','store':'joined',
            'pending_joins':0,'failed_joins':0,'uncertain_owners':0,'host_failure':None,
            'uncommitted_turns':0,'unresolved_turns':0,'store_failed':False,'blob_tasks':0,
            'unstarted_dispatchers':0,'unclosed_sessions':0,'unjoined_dispatchers':0}
        if any(type(value.get(key)) is not type(expected) or value.get(key)!=expected
               for key,expected in required.items()):
            raise Blocked('daemon idle exit record is not a clean idle shutdown')
        return required

    def _refresh_daemon(self,*,restart=True,idle_exit_pending=False):
        """Accept only a proven clean idle end, then identify a fresh private daemon (§13)."""
        if self.daemon is None: return
        identity=self.daemon; alive=self.proc.alive(identity)
        if alive is True:
            if not idle_exit_pending:return
            def owned_release():
                holders=self._lock_rows()
                if set(holders)!=set(self._lock_paths()) or any(
                        rows not in ([],[(identity.pid,'FLOCK')]) for rows in holders.values()):
                    raise Blocked('CLI daemon generation lock ownership changed')
            self._await_gone([identity],'daemon idle exit did not complete',guard=owned_release)
            alive=self.proc.alive(identity)
        if alive is not False:
            raise safety.process_block('daemon',alive,'owned process is not verified alive')
        try:
            if not self._locks_free(): raise Blocked('daemon idle exit locks not released')
            state=self.state.lstat()
            if not stat.S_ISDIR(state.st_mode) or state.st_uid!=os.getuid() \
                    or stat.S_IMODE(state.st_mode)!=0o700:
                raise Blocked('daemon idle exit state unsafe')
            path=self.state/'store.sqlite3'
            info=path.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_uid!=os.getuid() or info.st_nlink!=1 \
                    or stat.S_IMODE(info.st_mode)!=0o600:
                raise Blocked('daemon idle exit Store unsafe')
            with contextlib.closing(sqlite3.connect(f'file:{path}?mode=ro',uri=True,timeout=1)) as store:
                open_turns=store.execute("SELECT count(*) FROM turns WHERE ended_seq IS NULL "
                    "OR state NOT IN ('completed','failed','cancelled','unknown')").fetchone()[0]
            if open_turns!=0: raise Blocked('daemon idle exit has open accepted work')
            summary=self._idle_exit_summary()
            after=self.proc.alive(identity)
            if after is not False:
                raise safety.process_block('daemon',after,'daemon idle exit ownership changed')
            if not self._locks_free(): raise Blocked('daemon idle exit ownership changed')
        except (OSError,sqlite3.Error) as error:
            raise safety.process_block('daemon',False,'daemon idle exit evidence unavailable') from error
        except Blocked as error:
            if not hasattr(error,'process_liveness'):
                error.process_role='daemon'; error.process_liveness=False
            raise
        record=self._record('daemon-generation',{'event':'clean-idle-exit',
            'generation':len(self.daemon_generations),**identity.report(),
            'locks_released':True,'open_turns':open_turns,'shutdown':summary})
        if self.daemon_generations:
            self.daemon_generations[-1].update(clean_idle_exit=True,exit_record=record)
        self.close_event_capture()
        self.daemon=None; self.anchor=None; self.vendor_identity=None; self._http=None
        self._verified_host_record=None; self._event_generation=None
        self._bootstrap_session=None; self._daemon_log_mark=None
        self._armed={}; self._armed_history=set(); self._seam_targets={}
        if restart: self.start(self.phase_kind,self.project)

    def _lock_paths(self): return (self.runtime/'daemon.lock',self.state/'store.lock')

    def _lock_rows(self):
        try: raw=(self.proc.root/'locks').read_bytes()
        except OSError as error: raise Blocked('private lock evidence unreadable') from error
        if len(raw)>OBSERVATION_BYTES: raise Blocked('lock evidence exceeds bound')
        ids={}
        for path in self._lock_paths():
            try: row=path.lstat()
            except OSError as error: raise Blocked('private lock file absent') from error
            if not stat.S_ISREG(row.st_mode) or row.st_uid!=os.getuid() or row.st_nlink!=1:
                raise Blocked('private lock file identity unverified')
            ids[f'{os.major(row.st_dev):02x}:{os.minor(row.st_dev):02x}:{row.st_ino}']=path
        result={path:[] for path in self._lock_paths()}
        for line in raw.decode('ascii','strict').splitlines():
            fields=line.replace('-> ','').split()
            if len(fields)>=6 and fields[5] in ids:
                result[ids[fields[5]]].append((int(fields[4]),fields[1]))
        return result

    def _locks_owned(self,pid):
        return all(rows==[(pid,'FLOCK')] for rows in self._lock_rows().values())

    def _bind_cli_generation(self):
        """§13: a served reply must belong to the registered private daemon."""
        if self.daemon is None:
            if self._daemon_start_status:return  # start() verifies and registers this status.
            raise Blocked('CLI requires registered daemon generation')
        identity=self.daemon;holders=self._lock_rows()
        if set(holders)!=set(self._lock_paths()):
            raise Blocked('CLI daemon generation lock ownership changed')
        if any((pid,kind)!=(identity.pid,'FLOCK') for rows in holders.values() for pid,kind in rows):
            raise Blocked('CLI daemon generation lock ownership changed')
        alive=self.proc.alive(identity)
        if alive is False:
            # Only clean idle evidence can end this generation; never adopt an
            # unseen holder or attach another generation's reply to this one.
            self._refresh_daemon(restart=True)
            return
        if alive is not True:
            raise safety.process_block('daemon',alive,'CLI daemon generation unverifiable')
        if not all(rows==[(identity.pid,'FLOCK')] for rows in holders.values()):
            self._refresh_daemon(restart=True,idle_exit_pending=True)
            return
        self._verify(identity,'daemon')

    def _discover_private_lockers(self,*,strict=False):
        """§13: retain unseen daemon generations by private locks/HOME, never by name."""
        try:
            holders=self._lock_rows()
            candidates=sorted({(pid,kind) for rows in holders.values() for pid,kind in rows})
            for pid,kind in candidates:
                if kind!='FLOCK' or pid<=0: raise Blocked('private locker kind unverifiable')
                row=self.proc.stat(pid)
                if row is None: raise Blocked('private locker identity unreadable')
                identity=safety.Identity(pid,row['start_ticks'])
                if any(record['pid']==pid and record['start_ticks']==identity.start_ticks
                       for record in self.daemon_generations):
                    # Already proved owned. Its designed shutdown may release
                    # either lock before process death; absence is polled below.
                    if self.proc.alive(identity) is None:
                        raise safety.process_block('daemon',None,'private locker identity unreadable')
                    continue
                self._verify(identity,'daemon')
                info=(self.proc.root/str(pid)).stat()
                if info.st_uid!=os.getuid(): raise Blocked('private locker owner unverified')
                # Read only HOME's value, with descriptor-bound identity checks;
                # no other environment value is sliced, logged or persisted.
                raw=self.proc.read(identity,'environ')
                home=[];offset=0
                while offset<len(raw):
                    end=raw.find(b'\0',offset)
                    if end<0:break
                    if raw.startswith(b'HOME=',offset):home.append(raw[offset+5:end])
                    offset=end+1
                valid=raw.endswith(b'\0') and home==[os.fsencode(self.env['HOME'])]
                del raw,home
                if not valid: raise Blocked('private locker HOME unverified')
                for directory in (self.state,Path(self.env['HOME'])):
                    info=directory.lstat()
                    if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.getuid() \
                            or stat.S_IMODE(info.st_mode)!=0o700:
                        raise Blocked('private daemon directory identity unverified')
                again=self._lock_rows()
                if not any((pid,'FLOCK') in rows for rows in again.values()):
                    if self.proc.alive(identity) is not False:
                        raise Blocked('private locker ownership changed')
                self._verify(identity,'daemon')
                self.identities.add(identity)
                if not any(record['pid']==pid and record['start_ticks']==identity.start_ticks
                           for record in self.daemon_generations):
                    record={'generation':len(self.daemon_generations)+1,'pid':pid,
                            'start_ticks':identity.start_ticks,'build':self.phase_kind,
                            'discovery':'private-locks-and-home'}
                    self.daemon_generations.append(record)
                    self.ownership.daemon_started(self.state)
                    self._record('daemon-generation',{'event':'cleanup-observed',**record})
        except (Blocked,OSError,KeyError) as error:
            if strict:
                if isinstance(error,Blocked): raise
                raise Blocked('private locker observation unverifiable') from error
            self.uncertain.append('private lockers unverifiable')

    @reply_check
    def via(self,args):
        """Attach the fixed CLI verb to any runner block before cleanup (§13)."""
        verb=self._cli_verb(args)
        with safety.block_context(verb=verb):
            return self._via(args)

    @staticmethod
    def _cli_verb(args):
        """§13: CLI diagnostic routes contain only fixed verbs."""
        verbs={'daemon','describe','models','spawn','resume','wait','result','cancel','close',
               'steer','events','status','logs'}
        return args[0] if args and args[0] in verbs else 'unknown'

    def _via(self,args):
        """Call selected private CLI; bearer handles use stdin and replies stay in memory."""
        if not self._binary: raise Blocked('VIA build not selected')
        args=list(args)
        if not args: raise Blocked('CLI verb missing')
        verb=args[0]
        # §13: probes and submitted turns can cause Host to launch a vendor or successor.
        if verb in {'describe','models','spawn','resume','steer'}:
            self._check_ancestors('cli-'+verb)
        if self.daemon is not None and not (verb=='daemon' and len(args)>1 and args[1]=='stop'):
            self._refresh_daemon()
        if self.daemon is None and not (self._daemon_start_status and args[:2]==['daemon','status']):
            raise Blocked('CLI requires registered daemon generation')
        if verb in {'spawn','resume','steer'} and '--max-steps' not in args:
            # §13 allows only the exact held mock bootstrap and the fixed L11
            # credential-fence probe or verified missing-ID mock refusal control.
            # L11's unoffered effort (§5) is a second
            # fence before native session creation. Ordinary turns always check.
            if args!=self._metadata_command and args!=self._bootstrap_command:
                self.spending_check(args=args)
                self._admit_model(self.last_model)
        if '--json' not in args: args.append('--json')
        cleanup=(verb=='daemon' and len(args)>1 and args[1]=='stop') or (
            self._bootstrap_cleanup and verb in {'cancel','close'})
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
                    prompt_path=self.ownership.register(self.work/('prompt-'+secrets.token_hex(8)+'.tmp'),
                                                        'runner-evidence',directory=False)
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
        if verb=='cancel' and '--wait' in args:
            grace=CANCEL_ACK_SECONDS
            if '--force-after' in args:
                try:grace=int(args[args.index('--force-after')+1])/1000
                except (ValueError,IndexError) as error:raise Blocked('invalid CLI cancel grace') from error
                if not 0<=grace<=FOREGROUND_WAIT_SECONDS:raise Blocked('CLI cancel grace exceeds phase ceiling')
            timeout=grace+CANCEL_CLEANUP_SECONDS+WAIT_REPLY_MARGIN_SECONDS
        if verb=='close':
            timeout=CLOSE_REPLY_SECONDS+WAIT_REPLY_MARGIN_SECONDS
            if '--deadline-ms' in args:
                try:close_deadline=int(args[args.index('--deadline-ms')+1])/1000
                except (ValueError,IndexError) as error:raise Blocked('invalid CLI close deadline') from error
                if not 0<=close_deadline<=FOREGROUND_WAIT_SECONDS:
                    raise Blocked('CLI close deadline exceeds phase ceiling')
                timeout=close_deadline+30+WAIT_REPLY_MARGIN_SECONDS
        if verb=='spawn' and '--background' not in args:
            timeout=FOREGROUND_WAIT_SECONDS+WAIT_REPLY_MARGIN_SECONDS
        if verb=='wait':
            cli_timeout=180000
            if '--timeout-ms' in args:
                try: cli_timeout=int(args[args.index('--timeout-ms')+1])
                except (ValueError,IndexError) as error: raise Blocked('invalid CLI wait timeout') from error
                if not 0<=cli_timeout<=180000: raise Blocked('CLI wait timeout exceeds phase ceiling')
            timeout=cli_timeout/1000+WAIT_REPLY_MARGIN_SECONDS
        if self.phase_deadline and not cleanup: timeout=min(timeout,max(0.01,self.phase_deadline-time.monotonic()))
        if self._bootstrap_active and self._bootstrap_deadline is not None and not cleanup:
            remaining=self._bootstrap_deadline-time.monotonic()
            if remaining<=0: raise Blocked('bootstrap CLI deadline')
            timeout=min(timeout,remaining)
        observation_deadline=getattr(self,'_observation_deadline',None)
        if observation_deadline is not None and not cleanup:
            remaining=observation_deadline-time.monotonic()
            if remaining<=0: raise Blocked('owned observation CLI deadline')
            timeout=min(timeout,remaining)
        try:
            command_started=time.monotonic()
            submitted_model=self.last_model;submitted_generation=self.vendor_identity
            def observe(rc,out,err,**metadata):
                self.reply_evidence.capture('via',self._cli_verb(args),out,exit_code=rc,**metadata)
                if err: self.reply_evidence.capture('via-stderr',self._cli_verb(args),err,exit_code=rc,**metadata)
            observers={'observe':observe} if self.execute is bounded_command else {}
            rc,out,err=self.execute([str(self._binary),*args],env=self.env,cwd=self.project,
                                    input=input,timeout=timeout,**observers)
            if not (verb=='daemon' and len(args)>1 and args[1]=='stop'):
                self._bind_cli_generation()
            if not observers: observe(rc,out,err,complete=True)
        finally:
            if prompt_path is not None: prompt_path.unlink(missing_ok=True)
        if self.vault.leaks(out) or self.vault.leaks(err) or any(form in out+err for form in self.secret_forms):
            raise Blocked('secret found in CLI output; output not persisted')
        if rc==4: raise Blocked('private daemon unreachable')
        if rc not in {0,3}:
            value=strict_reply(err or out,'error')
            self._record('cli-error',{'verb':self._cli_verb(args),'rc':rc,'typed':True})
            # C1 §1 CLI stderr is the error object; keep the internal wrapper.
            return {'cli_error':{'error':value},'exit_code':rc}
        schema={'status':'status','wait':'envelope','result':'envelope','events':'events page','logs':'logs',
                'cancel':'cancel reply','close':'close'}.get(verb,'unknown')
        if verb=='daemon' and len(args)>1:
            schema='daemon status' if args[1]=='status' else 'daemon stop'
        if verb in {'models','describe'}: schema=verb
        if verb=='resume': schema='receipt'
        if verb=='spawn': schema='spawn receipt' if '--background' in args else 'envelope'
        receipt=None
        if verb=='spawn' and '--background' not in args:
            if len(out)>OBSERVATION_BYTES: raise Blocked('CLI response exceeds observation bound')
            lines=out.splitlines()
            if len(lines)!=2: raise Blocked('foreground spawn requires receipt and envelope')
            receipt=strict_reply(lines[0],'spawn receipt')
            value=strict_reply(lines[1],'envelope')
            address=value['session_id']+'/'+str(value['turn'])
            if receipt['session_id']!=value['session_id'] or receipt['turn']!=address:
                raise Blocked('foreground spawn receipt and envelope identity differ')
        else:
            value=strict_reply(out,schema)
        bearer=receipt if receipt is not None else value
        if 'handle' in bearer:
            if type(bearer.get('session_id')) is not str or type(bearer['handle']) is not str:
                raise Blocked('bearer receipt malformed')
            self.handles[bearer['session_id']]=bearer['handle']
            self.secret_forms.append(bearer['handle'].encode())
            self.bearer_forms.add(bearer['handle'].encode())
        if verb in {'spawn','resume'}:
            timing=dict(receipt if receipt is not None else value)
            if verb=='resume':timing['session_id']=args[1]
            self._submitted_models[timing['session_id']]=submitted_model
            self._timeline_submit(timing,submitted_model,command_started,submitted_generation)
        if schema=='envelope':
            if self.diagnostic_mock_only and type(value['vendor_session_id']) is str:
                sid=value['vendor_session_id']
                model=self._submitted_models.get(value['session_id'])
                if model!=safety.MOCK_IDENTITY:raise Blocked('mock diagnostic session identity unverifiable')
                fixture=next((name for name,row in self.fixtures.items() if row['path']==self.project),None)
                self._diagnostic_sessions[sid]={'model':model,'fixture':fixture,'project':self.project}
            if self._timelines:
                self._timeline_bind(value['session_id'],value['vendor_session_id'],turn=value['turn'])
            self.account(value)
        if receipt is not None: self.owned_replies.append(receipt)
        self.owned_replies.append(value)
        if verb in {'spawn','resume'}:
            internal=dict(receipt if receipt is not None else value)
            if verb=='resume': internal['session_id']=args[1]
            self.last_request={'verb':verb,'receipt':internal,'prompt':prompt,'args':args,
                               'model':submitted_model}
        if verb=='cancel': self.last_cancel={'at':time.monotonic(),'reply':value,'held':dict(self._armed),
                                            'elapsed_ms':int((time.monotonic()-command_started)*1000)}
        self._record('cli',{'verb':self._cli_verb(args),'rc':rc,'schema':schema,'fields':sorted(value),
                            'handle_persisted':False,'stderr_present':bool(err)})
        if self.inventory: self.inventory.check()
        # The bootstrap lease keeps acquisition alive until a real successor
        # session has attached. Then close it so retirement/session counts are
        # governed by the case's sessions, not the acquisition helper.
        bootstrap=self._bootstrap_session
        target=value.get('session_id')
        if bootstrap and not self._bootstrap_active and target!=bootstrap and target in self.handles:
            if schema=='envelope' and type(value.get('vendor_session_id')) is str:
                # A historical envelope is not proof of a current lease. The
                # status path below confirms generation and open admission.
                self.via(['status',target])
            elif verb=='status' and value.get('vendor_identity_verified') is True \
                    and value.get('admission')=='open' and value.get('state') in {'idle','active'} \
                    and type(value.get('vendor_session_id')) is str:
                self._bootstrap_session=None
                closed=self.via(['close',bootstrap])
                if closed.get('state')!='closed': raise Blocked('bootstrap lease handoff failed')
        return value

    def _host_record(self,*,allow_absent=False):
        """§13: retry only an owned, verified predecessor's pending absence commit."""
        path=self.state/'store.sqlite3'
        deadline=min(time.monotonic()+OWNED_READINESS_SECONDS,
                     self.phase_deadline if self.phase_deadline is not None else float('inf'),
                     self._bootstrap_deadline if self._bootstrap_active and self._bootstrap_deadline is not None
                     else float('inf'), self._observation_deadline if self._observation_deadline is not None
                     else float('inf'))
        successor=None; binding=None; daemon=self.daemon; state_root=self.state
        while True:
            try:
                with contextlib.closing(sqlite3.connect(f'file:{path}?mode=ro',uri=True,timeout=1)) as connection:
                    rows=connection.execute('SELECT generation,pid,start_ticks,vendor_pid,phase,owner_server '
                                            'FROM anchors WHERE owner_server IS NOT NULL '
                                            'AND absence_time IS NULL').fetchall()
            except (sqlite3.Error,OSError) as error:
                raise Blocked('owned Host launch record unavailable') from error
            if successor is not None:
                if self.daemon!=daemon or self.state!=state_root:
                    raise Blocked('owned Host handover daemon changed')
                self._verify(daemon)
                if self.signals: self.signals.guard()
                if time.monotonic()>=deadline: raise Blocked('owned Host predecessor absence deadline')
                if not any(row[0]==successor for row in rows):
                    raise Blocked('owned Host handover successor changed')
            if len(rows)<=1: break
            pending=self._owned_host_handover(rows)
            if pending is None: raise Blocked('owned Host generation ambiguous')
            if successor is not None and pending['generation']!=successor:
                raise Blocked('owned Host handover successor changed')
            if binding is not None and any(binding[key] is not None and pending[key]!=binding[key]
                                          for key in ('server_id','anchor','vendor')):
                raise Blocked('owned Host handover successor changed')
            binding=pending; successor=pending['generation']
            if time.monotonic()>=deadline: raise Blocked('owned Host predecessor absence deadline')
            time.sleep(min(.01,max(0,deadline-time.monotonic())))
        if not rows:
            if allow_absent: return None
            raise Blocked('owned Host generation absent')
        generation,pid,ticks,vendor_pid,phase,server_id=rows[0]
        if binding is not None:
            if server_id!=binding['server_id'] or (binding['anchor'] is not None
                    and (pid,ticks)!=(binding['anchor'].pid,binding['anchor'].start_ticks)) or (binding['vendor'] is not None
                    and vendor_pid!=binding['vendor'].pid):
                raise Blocked('owned Host handover successor changed')
            for identity in (binding['anchor'],binding['vendor']):
                if identity is not None: self._verify(identity)
        # One not-yet-armed intent is pending; never filter it out of an
        # ambiguity check or retry an unreadable/invalid Store.
        if any(value is None for value in (pid,ticks,vendor_pid)):
            if allow_absent: return None
            raise Blocked('owned Host child identity not yet recorded')
        return {'generation':generation,'anchor':safety.Identity(pid,ticks),
                'vendor_pid':vendor_pid,'phase':phase,'server_id':server_id}

    def _owned_host_handover(self,rows):
        """§13: private Host intent plus captured predecessor identity, never a row-count guess."""
        known=self._verified_host_record
        if len(rows)!=2 or known is None or known['state_root']!=self.state or self.daemon is None:
            return None
        self._verify(self.daemon)
        predecessor=(known['generation'],known['anchor'].pid,known['anchor'].start_ticks,
                     known['vendor_pid'],known['phase'],known['server_id'])
        if rows.count(predecessor)!=1: return None
        current=next(row for row in rows if row!=predecessor)
        generation,pid,ticks,vendor_pid,phase,server=current
        if type(generation) is not str or not generation or generation==known['generation'] \
                or type(server) is not str or not server or server==known['server_id'] \
                or phase not in {'intent','identified','arm_intent'}:
            return None
        # These are the private daemon's Host intents, not foreign live rows.
        # Once an identity is durable it must still match its owned ancestry.
        try:
            state=self.state.lstat(); store=(self.state/'store.sqlite3').lstat()
        except OSError as error:
            raise Blocked('owned Host handover Store unsafe') from error
        if not stat.S_ISDIR(state.st_mode) or stat.S_IMODE(state.st_mode)!=0o700 \
                or not stat.S_ISREG(store.st_mode) or stat.S_IMODE(store.st_mode)!=0o600 \
                or state.st_uid!=os.getuid() or store.st_uid!=os.getuid():
            raise Blocked('owned Host handover Store unsafe')
        anchor=None; vendor=None
        if pid is None or ticks is None:
            if phase!='intent' or any(value is not None for value in (pid,ticks,vendor_pid)):
                return None
        else:
            anchor=safety.Identity(pid,ticks); self._verify(anchor)
            row=self.proc.stat(pid)
            if row is None or row['start_ticks']!=ticks or row['ppid']!=self.daemon.pid:
                raise Blocked('owned Host successor ancestry changed')
            self.identities.add(anchor)
            if vendor_pid is not None:
                row=self.proc.stat(vendor_pid)
                if row is None or row['ppid']!=pid:
                    raise Blocked('owned Host successor ancestry changed')
                vendor=safety.Identity(vendor_pid,row['start_ticks']); self._verify(vendor)
                self.identities.add(vendor)
        states=[self.proc.alive(identity) for identity in (known['anchor'],known['vendor'])]
        if any(value is None for value in states):
            role='anchor' if states[0] is None else 'vendor'
            raise safety.process_block(role,None,'owned Host predecessor identity unverifiable')
        gone=all(value is False for value in states)
        going=self._killed_anchor==known['anchor'] and all(type(value) is bool for value in states)
        return {'generation':generation,'server_id':server,'anchor':anchor,'vendor':vendor} if gone or going else None

    def ensure_vendor(self):
        """Acquire through one held mock turn, then authenticate the Host child (§13)."""
        if not self.daemon: raise Blocked('owned daemon required')
        self._refresh_daemon()
        self._verify(self.daemon)
        if self._http and self.vendor_identity:
            alive=self.proc.alive(self.vendor_identity)
            if alive is None: raise safety.process_block('vendor',None,'owned vendor identity unverifiable')
            if alive is True:
                self._owned_observation_guard()()
                return
        if self._bootstrap_active: raise Blocked('recursive bootstrap acquisition')
        record=self._host_record(allow_absent=True)
        if record is not None:
            row=self.proc.stat(record['vendor_pid'])
            if row is None: raise safety.process_block('vendor',None,'owned vendor identity unverifiable')
            identity=safety.Identity(row['pid'],row['start_ticks'])
            alive=self.proc.alive(identity)
            if alive is None: raise safety.process_block('vendor',None,'owned vendor identity unverifiable')
            if alive is True:
                self._observe_vendor()
                return
        self._bootstrap_vendor()

    def _listener_origin(self, record, identity):
        """§13/§2: await startup under the existing bound; never retry uncertain ownership."""
        started=time.monotonic(); deadline=started+BOOTSTRAP_SECONDS
        if self.phase_deadline is not None: deadline=min(deadline,self.phase_deadline)
        if self._bootstrap_active and self._bootstrap_deadline is not None:
            deadline=min(deadline,self._bootstrap_deadline)
        binding=tuple(record[key] for key in ('generation','anchor','vendor_pid','server_id'))
        checks=0; ready=False
        try:
            while True:
                if self.signals: self.signals.guard()
                if time.monotonic()>=deadline: raise Blocked('owned listener startup deadline')
                current=self._host_record()
                if tuple(current[key] for key in ('generation','anchor','vendor_pid','server_id'))!=binding:
                    raise Blocked('owned listener generation changed while starting')
                self._verify(record['anchor']); self._verify(identity)
                checks+=1
                try:
                    origin=self.proc.listener(identity)
                except safety.ListenerNotReady:
                    time.sleep(min(.02,max(0,deadline-time.monotonic())))
                    continue
                if time.monotonic()>=deadline: raise Blocked('owned listener startup deadline')
                ready=True
                return origin
        finally:
            self._record('vendor-listener',{'owned_pid':identity.pid,'checks':checks,'ready':ready,
                'elapsed_ms':int((time.monotonic()-started)*1000)})

    def _owned_observation_guard(self):
        """§13: retain the original generation and PID/ticks across every readiness poll."""
        identities=tuple(getattr(self,key,None) for key in ('daemon','anchor','vendor_identity'))
        binding=None
        def verify():
            nonlocal binding
            current=tuple(getattr(self,key,None) for key in ('daemon','anchor','vendor_identity'))
            if current!=identities: raise Blocked('owned observation generation changed')
            for identity in identities:
                if identity is not None: self._verify(identity)
            if identities[1] is not None and identities[2] is not None:
                record=self._host_record()
                value=tuple(record[key] for key in ('generation','anchor','vendor_pid','server_id'))
                if record['anchor']!=identities[1] or record['vendor_pid']!=identities[2].pid:
                    raise Blocked('owned observation generation changed')
                if binding is not None and value!=binding:
                    raise Blocked('owned observation generation changed')
                binding=value
        return verify

    def _await_owned(self,read,reason,*,seconds=OWNED_READINESS_SECONDS,deadline=None,interval=.01):
        """§13: only explicit None is pending; every poll retains identity and its bound."""
        end=min(time.monotonic()+seconds,deadline if deadline is not None else float('inf'),
                self.phase_deadline if self.phase_deadline is not None else float('inf'),
                self._bootstrap_deadline if self._bootstrap_active and self._bootstrap_deadline is not None
                else float('inf'))
        verify=self._owned_observation_guard()
        saved=getattr(self,'_observation_deadline',None)
        if saved is not None: end=min(end,saved)
        self._observation_deadline=end
        try:
            while True:
                if self.signals: self.signals.guard()
                if time.monotonic()>=end: raise Blocked(reason)
                verify()
                value=read()
                verify()
                if time.monotonic()>=end: raise Blocked(reason)
                if value is not None: return value
                time.sleep(min(interval,max(0,end-time.monotonic())))
        finally:
            self._observation_deadline=saved

    def _await_native(self,sid,ready,reason):
        """§13: await a correlated native fact; SSE snapshots alone imply no synchronization."""
        def read():
            rows=self.observe('native_events',sid)['events']
            return rows if ready(rows) else None
        return self._await_owned(read,reason)

    def _await_gone(self,identities,reason,*,seconds=OWNED_READINESS_SECONDS,guard=None):
        """§13: live with verified ticks is pending; unverifiable absence is never retried."""
        end=time.monotonic()+seconds
        if self.phase_deadline is not None: end=min(end,self.phase_deadline)
        while True:
            if self.signals: self.signals.guard()
            if guard is not None:guard()
            states=[self.proc.alive(identity) for identity in identities]
            if any(state is None for state in states):
                identity=identities[states.index(None)]
                raise safety.process_block(self._process_role(identity),None,
                                           'owned disappearance identity unverifiable')
            if all(state is False for state in states): return
            if time.monotonic()>=end: raise Blocked(reason)
            time.sleep(min(.01,max(0,end-time.monotonic())))

    def _live_pin(self,session):
        """§13 L14/marker: a tool spawn can precede VIA's acceptance commit."""
        def read():
            value=self.via(['status',session]); active=value.get('active_turn')
            if self._queued_status(value,session):return None
            if value.get('state')!='active' or value.get('admission')!='open' or type(active) is not dict:
                raise Blocked('live turn pin ended or admission changed')
            phase=active.get('phase')
            if active.get('state')!='running' or phase not in {'submitting','accepted'}:
                raise Blocked('live turn phase unrecognized')
            if phase=='accepted' and value.get('vendor_identity_verified') is True: return value
            return None
        return self._await_owned(read,'live turn pin acceptance deadline')

    def _queued_status(self,value,session):
        """C1 §§3.2,3.7,7.2: only this receipted queued turn is pending dispatch."""
        if value.get('state') not in {'idle','active'} or value.get('admission')!='open' \
                or value.get('session_id')!=session or value.get('active_turn') is not None:
            return False
        receipt=(self.last_request or {}).get('receipt',{})
        address=receipt.get('turn')
        if receipt.get('session_id')!=session or type(address) is not str \
                or not address.startswith(session+'/'):return False
        try:number=int(address.rsplit('/',1)[1])
        except ValueError:return False
        turns=value.get('turns')
        if number<1 or type(turns) is not list:return False
        matching=[row for row in turns if type(row) is dict and type(row.get('n')) is int and row['n']==number]
        return len(matching)==1 and matching[0].get('state')=='queued'

    def _await_helper_point(self,helper,point):
        """§13 L14: wait for an atomic update of the already captured helper identity."""
        path=self._helper_folder(self.project)/(helper+'.json')
        expected=[identity for identity,row in self.helper_ledger.items()
                  if row.get('kind')==helper and self.helper_generations.get(identity)==self.vendor_identity]
        if len(expected)!=1: raise Blocked('helper after-point identity ambiguous')
        identity=expected[0]
        def read():
            try: raw=path.read_bytes()
            except FileNotFoundError: return None
            if len(raw)>OBSERVATION_BYTES: raise Blocked('helper observation bound')
            value=self._json(raw)
            _typed(value,{'pid':I,'start_ticks':I,'point':S},'helper after-point')
            if safety.Identity(value['pid'],value['start_ticks'])!=identity:
                raise Blocked('helper after-point identity changed')
            alive=self.proc.alive(identity)
            if alive is None: raise safety.process_block('helper',None,'helper after-point identity unverifiable')
            if value['point']==point: return value
            if alive is False: raise safety.process_block('helper',False,'helper ended before after-point publication')
            return None
        return self._await_owned(read,'L14 helper after-spawn point not reached')

    @reply_check
    def _observe_vendor(self):
        """Authenticate pid/start ticks, owned listener and §2 handshake before use."""
        record=self._host_record(); self._verify(record['anchor'],'anchor')
        row=self.proc.stat(record['vendor_pid'])
        if row is None or row['ppid']!=record['anchor'].pid:
            raise Blocked('vendor is not Host anchor child')
        identity=safety.Identity(row['pid'],row['start_ticks']); self._verify(identity,'vendor')
        previous=self.vendor_identity
        password=self.vault.read_once(self.proc,identity)
        origin=self._listener_origin(record,identity)
        self._http=safety.OwnedHTTP(origin,identity,self.proc,password,
                                    deadline=self._bootstrap_deadline if self._bootstrap_active else self.phase_deadline)
        deadline=time.monotonic()+OWNED_READINESS_SECONDS
        binding=tuple(record[key] for key in ('generation','anchor','vendor_pid','server_id'))
        while True:
            if self.phase_deadline is not None: deadline=min(deadline,self.phase_deadline)
            if self._bootstrap_active and self._bootstrap_deadline is not None:
                deadline=min(deadline,self._bootstrap_deadline)
            if self.signals: self.signals.guard()
            if time.monotonic()>=deadline: raise Blocked('owned vendor info startup deadline')
            current=self._host_record()
            if tuple(current[key] for key in ('generation','anchor','vendor_pid','server_id'))!=binding:
                raise Blocked('owned vendor info generation changed')
            self._verify(record['anchor']); self._verify(identity)
            if self.proc.listener(identity)!=origin:
                raise Blocked('owned vendor info listener changed')
            status,raw=self._request('GET','/api/info',timeout=deadline-time.monotonic())
            value=self._json(raw)
            _typed(value,{'pid':I,'version':S},'vendor info')
            matches=value['pid']==identity.pid and value['version']=='2.0.22'
            self._record('vendor-info',{'status':status,'identity_version_match':matches,
                                       'body_sha256':hashlib.sha256(raw).hexdigest()})
            if not matches or status not in {200,503}:
                raise Blocked('owned vendor handshake mismatch')
            if status==200: break
            # §13/§2: the checked child reports 503 while its database starts.
            # Only this verified not-ready reply retries; admission still needs 200.
            time.sleep(min(.02,max(0,deadline-time.monotonic())))
        self.vendor_identity=identity; self.anchor=record['anchor']
        self._verified_host_record={**record,'state_root':self.state,'vendor':identity}
        self.server_identities.add(identity)
        self.identities.update((identity,self.anchor))
        rotated=None if previous is None else self.vault.changed(previous,identity)
        self._last_rotation=rotated
        self._record('vendor-auth',{'owned_pid':identity.pid,'version':value['version'],
                                    'password_changed':rotated,'password_written':False})
        if self._event_generation!=identity:
            self.start_event_capture(); self._event_generation=identity

    def _bootstrap_static(self,config):
        """Check the credential-free namespace, private environment and mock selectors (§13)."""
        allowed={*safety.PRIVATE_PARTS,'PATH','LANG','VIA_STATE_DIR','VIA_RUNTIME_DIR'}
        if self.phase_kind=='failpoints': allowed|={'VIA_FAILPOINT_DIR','VIA_FAILPOINT_TOKEN'}
        if set(self.env)!=allowed or set(self.namespace_env)!=set(safety.PRIVATE_PARTS):
            raise Blocked('bootstrap environment allow-list failed')
        safety.validate_path(self.env['PATH'],self.helpers)
        for key in safety.PRIVATE_PARTS:
            via_root=Path(self.env[key])
            owner=self.ownership.classify(via_root)
            if owner is None or owner.kind!='via-owned' or via_root.is_symlink() or not via_root.is_dir():
                raise Blocked('bootstrap daemon environment is not private')
            root=Path(self.namespace_env[key])
            if not root.is_relative_to(self.namespace) or root.is_symlink() or not root.is_dir():
                raise Blocked('bootstrap namespace environment is not private')
        if config.get('model')!=safety.MOCK_IDENTITY or set(config.get('providers',{}))!={'oclive-mock'}:
            raise Blocked('bootstrap frozen mock model missing')
        row=config['providers']['oclive-mock']
        if set(config)!={'providers','model','agents'} \
                or set(row)!={'name','package','env','settings','models'} \
                or row.get('env')!=[] or row.get('package')!='@ai-sdk/openai-compatible' \
                or set(row.get('settings',{}))!={'baseURL','apiKey'} \
                or row['settings']['apiKey']!='fixture-unused' \
                or set(row.get('models',{}))!={'fixture-free'} \
                or row['models']['fixture-free'].get('cost')!={'input':0,'output':0,'cache':{'read':0,'write':0}}:
            raise Blocked('bootstrap provider credential-free shape failed')
        required={'via','title','summary','compaction','explore','general','build','plan'}
        agents=config.get('agents',{})
        if set(agents)!=required or any(row!={'model':safety.MOCK_IDENTITY} for row in agents.values()):
            raise Blocked('bootstrap auxiliary model selectors are not frozen')
        endpoints=self._validate_provider_config(config)
        path=self.namespace/'opencode.json'
        if path.is_symlink() or not path.is_file() or path.stat().st_size>OBSERVATION_BYTES \
                or self._json(path.read_bytes())!=config:
            raise Blocked('bootstrap namespace configuration drift')
        self._admit_model(safety.MOCK_IDENTITY)
        return endpoints

    @reply_check
    def _bootstrap_vendor(self):
        """Labelled mock-only acquisition; failed proof cancels without releasing (§13)."""
        if self.guard.stopped: raise Blocked('spending control previously failed')
        self._check_ancestors('bootstrap')
        from opencode_cases import ResponseHold, EvidenceUnavailable
        deadline=min(time.monotonic()+BOOTSTRAP_SECONDS,self.phase_deadline or float('inf'))
        hold=ResponseHold(deadline)
        saved=(self.project,self.provider_endpoints,self.last_model,self.last_request)
        receipt=None; provider=None; checked=False; cancelled=False; cleanup_failed=False
        requests_before=0
        self._bootstrap_count+=1
        name='bootstrap-'+str(self._bootstrap_count)
        try:
            cached=self._namespace_bootstraps.get(self.namespace)
            # The sentinel may intentionally replace the namespace project
            # config. Reuse that registered fixture's provider; bootstrap
            # bookkeeping must never mask the case's effective configuration.
            namespace_fixture=next((row for row in self.fixtures.values()
                                    if row['path']==self.namespace),None)
            if namespace_fixture is not None:
                config=namespace_fixture['config']
                endpoints=self._validate_provider_config(config)
                matches=[value for value in self.mock_providers.values()
                         if value.endpoint==endpoints.get('oclive-mock')]
                if len(matches)!=1: raise Blocked('bootstrap namespace mock provider ambiguous')
                cached=(config,matches[0])
                self._namespace_bootstraps[self.namespace]=cached
            if cached is None:
                config=self._materialize_fixture(name,self.namespace,{'provider':'mock','response_hold':hold})
                provider=self.mock_providers[name]
                self._write_fixture_config(self.namespace,config,exclusive=True)
                self._namespace_bootstraps[self.namespace]=(config,provider)
            else:
                config,provider=cached
                provider.response_hold=hold
                requests_before=provider.requests
            self.project=self.namespace
            self._bootstrap_config=config
            self.provider_endpoints=self._bootstrap_static(config)
            command=['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                     '--cwd',str(self.namespace),'--bound','full','--network','--background',
                     '--label','qualification-bootstrap-mock-only','--prompt','Reply BOOTSTRAP.']
            self.last_model=safety.MOCK_IDENTITY
            self._bootstrap_command=command; self._bootstrap_active=True
            self._bootstrap_deadline=deadline
            receipt=self.via(command)
            if receipt.get('exit_code'): raise Blocked('bootstrap spawn refused')
            while True:
                if self.signals: self.signals.guard()
                if time.monotonic()>=deadline: raise Blocked('bootstrap owned-server deadline')
                self._verify(self.daemon)
                if self._host_record(allow_absent=True) is not None: break
                time.sleep(min(.02,max(0,deadline-time.monotonic())))
            self._observe_vendor()
            self.spending_check(args=command)
            # The session's frozen identity is served evidence too; the request
            # alone is insufficient. Status is bounded and carries no raw text.
            sid=self._vendor_sid(receipt['session_id'])
            self.spending_check(path='/api/session/'+urllib.parse.quote(sid,safe='')+'/prompt')
            if self.last_model!=safety.MOCK_IDENTITY: raise Blocked('bootstrap served model is not mock')
            if provider.admission_blocked or self.guard.stopped: raise Blocked('bootstrap mock admission failed')
            checked=True
            try: hold.release()
            except EvidenceUnavailable as error:
                raise Blocked('bootstrap response hold expired or aborted') from error
            self._bootstrap_deadline=None; self._http.deadline=self.phase_deadline
            envelope=self.via(['wait',receipt['turn'],'--timeout-ms',str(BOOTSTRAP_WAIT_MS)])
            if envelope.get('state')!='completed': raise Blocked('bootstrap mock turn did not complete')
            if provider.requests<=requests_before or not provider.model_matches:
                raise Blocked('bootstrap mock request unobserved')
            self._bootstrap_session=receipt['session_id']
        except BaseException:
            hold.abort()
            if receipt and type(receipt.get('session_id')) is str and type(receipt.get('turn')) is str:
                self._bootstrap_cleanup=True
                try:
                    reply=self.via(['cancel',receipt['session_id'],'--turn',receipt['turn'].rsplit('/',1)[1]])
                    cancelled=not reply.get('exit_code')
                except BaseException:
                    cleanup_failed=True
                finally: self._bootstrap_cleanup=False
            raise
        finally:
            self._bootstrap_command=None; self._bootstrap_active=False
            self._bootstrap_deadline=None
            if self._http is not None: self._http.deadline=self.phase_deadline
            self._bootstrap_config=None
            self.project,self.provider_endpoints,self.last_model,self.last_request=saved
            self._record('bootstrap',{'label':'qualification-bootstrap-mock-only','charged':'mock-only',
                'gate':False,'static_checks':receipt is not None,'served_checks':checked,
                'response_released':hold.released,'response_aborted':hold.aborted,
                'cancel_requested':cancelled,'cleanup_failed':cleanup_failed,
                'public_requests':0,'mock_requests':0 if provider is None else provider.requests-requests_before})

    @staticmethod
    def _json(raw):
        try: return json.loads(raw,parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
        except (ValueError,UnicodeError) as error:
            raise Blocked('vendor JSON shape unavailable') from error

    @reply_check
    def vendor(self,method,path,body=None):
        """Memory-only authenticated API to one positively verified owned generation."""
        if urllib.parse.urlsplit(path).path=='/api/credential':
            raise Blocked('credential read/write requires isolated direct seeding')
        if method.upper() in {'POST','PUT','PATCH'} and any(
                word in urllib.parse.urlsplit(path).path for word in ('/prompt','/compact','/fork')):
            self.spending_check(body=body,path=path)
            self._admit_model(self.last_model)
        else: self.ensure_vendor()
        self._http.deadline=self.phase_deadline
        status,raw=self._request(method,path,body)
        if self.vault.leaks(raw): raise Blocked('password in vendor response')
        value=None if not raw else self._json(raw)
        self._record('vendor-request',{'method':method,'status':status,
                                      'route':self._route_name(path),'bytes':len(raw)})
        return {'status':status,'body':value}

    @staticmethod
    def _route_name(path):
        parts=urllib.parse.urlsplit(path).path.split('/')
        return '/'.join('<id>' if part.startswith(('ses_','msg_','in_')) else part for part in parts)

    @reply_check
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

    @reply_check
    def _catalog(self,checked_identity=None):
        """Decode prices unchanged; retain an allow-listed view if proof fails (§13)."""
        if checked_identity is not None: self._approved_model(checked_identity)
        self._catalog_evidence=None
        project=Path(self.project)
        location=project.relative_to(self.work).as_posix() if project.is_relative_to(self.work) \
                 else 'unregistered'
        def protected(raw):
            return self.vault.leaks(raw) or any(form and form in raw
                for form in (*self.secret_forms,*self.bearer_forms))
        observation=catalog_record(None,None,location,checked_identity,protected)
        query=urllib.parse.urlencode({'location[directory]':str(project)})
        def read():
            nonlocal observation
            observation=catalog_record(None,None,location,checked_identity,protected)
            status,raw=self._request('GET','/api/model?'+query,
                timeout=max(.001,self._observation_deadline-time.monotonic()))
            observation=catalog_record(status,None,location,checked_identity,protected)
            body=self._json(raw)
            observation=catalog_record(status,body,location,checked_identity,protected)
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
            observed=result.get(checked_identity)
            observation['checked_model']['status']='absent' if observed is None else \
                'explicit-zero' if observed['free'] else 'not-explicit-zero'
            return result if result and (checked_identity is None or observed is not None) else None
        try:
            result=self._await_owned(read,'owned location catalog readiness deadline',
                interval=CATALOG_READY_POLL_SECONDS)
        except Blocked:
            self._record('catalog-block',observation)
            raise
        self._catalog_evidence=observation
        self.catalog_cache=result
        return result

    def _approved_model(self,identity):
        """§13: unapproved identities latch the stop before acquisition or polling."""
        allowed={safety.MOCK_IDENTITY} if self.diagnostic_mock_only else {safety.FREE_IDENTITY,safety.MOCK_IDENTITY}
        if identity not in allowed:
            self.guard.stopped=True
            raise Blocked('paid or unapproved model identity')

    @reply_check
    def spending_check(self,*,args=None,body=None,path=None,cost=None):
        """Recheck structure for every model-capable request, never infer zero cost."""
        identity=safety.FREE_IDENTITY
        if args and '--model' in args:
            identity=args[args.index('--model')+1]
            self._approved_model(identity)
        if type(body) is dict and 'model' in body:
            ref=body['model']; _typed(ref,{'providerID':S,'id':S},'direct requested model')
            self._approved_model(ref['providerID']+'/'+ref['id'])
        self.ensure_vendor()
        self._http.deadline=self.phase_deadline
        path_session=None
        if path:
            parts=urllib.parse.urlsplit(path).path.split('/')
            if len(parts)>=5 and parts[1:3]==['api','session']:
                path_session=parts[3]
                if not path_session.startswith('ses_'): raise Blocked('direct session identity invalid')
        if path_session:
            status,raw=self._request('GET','/api/session/'+path_session)
            value=self._json(raw)
            if status!=200 or type(value.get('data')) is not dict or value['data'].get('id')!=path_session:
                raise Blocked('direct session model readback unavailable')
            ref=value['data'].get('model'); _typed(ref,{'providerID':S,'id':S},'direct session model')
            identity=ref['providerID']+'/'+ref['id']
            self._approved_model(identity)
            if body and type(body) is dict and 'model' in body:
                ref=body['model']; _typed(ref,{'providerID':S,'id':S},'direct requested model')
                requested=ref['providerID']+'/'+ref['id']
                self._approved_model(requested)
                if requested!=identity: raise Blocked('direct request model override differs from frozen session')
        elif args and '--model' in args: identity=args[args.index('--model')+1]
        elif body and type(body) is dict and type(body.get('model')) is dict:
            ref=body['model']; identity=ref['providerID']+'/'+ref['id']
        elif args and len(args)>1 and args[1] in self.handles:
            sid=self._vendor_sid(args[1]); status,raw=self._request('GET','/api/session/'+sid)
            value=self._json(raw)
            if status!=200 or type(value.get('data')) is not dict: raise Blocked('session model readback unavailable')
            ref=value['data'].get('model')
            _typed(ref,{'providerID':S,'id':S},'session model')
            identity=ref['providerID']+'/'+ref['id']
        self._approved_model(identity)
        provider,sep,model=identity.partition('/')
        if not sep: raise Blocked('model identity incomplete')
        names=self.vault.names(self.vendor_identity)
        if self.inventory: self.inventory.check()
        catalog=self._catalog(identity)
        # Effective project config is constructed before start and immutable during turns.
        row=next((row for row in self.fixtures.values() if row['path']==self.project),None)
        config=None if row is None else row['config']
        if self._bootstrap_active: config=self._bootstrap_config
        endpoints={} if config is None else self._validate_provider_config(config)
        if endpoints!=self.provider_endpoints:
            raise Blocked('project provider configuration drift')
        bindings=self._auxiliary_bindings(identity)
        if provider not in endpoints and not self.automatic_models_proven:
            raise Blocked('automatic/auxiliary model selection has not been proved with mock')
        if provider in endpoints:
            self._last_auxiliary_bindings=bindings
        self.last_model=identity
        status,raw=self._request('GET','/api/integration')
        if status!=200: raise Blocked('integration check unavailable')
        integration=self._json(raw)
        try:
            return self.guard.check(integration=integration,environment_names=names,catalog=catalog,
                                    provider=provider,model=model,project_providers=endpoints,cost=cost)
        except Blocked:
            if self._catalog_evidence is not None:
                self._record('catalog-block',self._catalog_evidence)
            raise

    def _config_identity(self):
        """§13: a previous served map belongs to one owned state and process generation."""
        return self.state,self.daemon,self.anchor,self.vendor_identity

    @staticmethod
    def _endpoint_map(endpoints):
        """§13: compare provider IDs and owned loopback host:port, without URL paths."""
        from opencode_reply import origin_class
        return {name:origin_class(endpoint)['host_port'] for name,endpoint in endpoints.items()}

    def _write_fixture_config(self,project,config,*,exclusive=False):
        """§13: only a runner config write arms the narrowly bounded reload observation."""
        project=Path(project)
        expected=self._endpoint_map(self._validate_provider_config(config))
        binding=self._config_identity()
        prior=self._verified_endpoint_maps.get(project)
        previous=None
        if prior is not None and prior['identity']==binding and self.vendor_identity is not None:
            self._owned_observation_guard()()
            previous=dict(prior['endpoints'])
        flags=os.O_WRONLY|os.O_CREAT|os.O_NOFOLLOW|(os.O_EXCL if exclusive else os.O_TRUNC)
        fd=os.open(project/'opencode.json',flags,0o600)
        with os.fdopen(fd,'w') as output: json.dump(config,output,allow_nan=False)
        self._endpoint_reloads[project]={'identity':binding,'previous':previous,'expected':expected}

    def _effective_configuration(self,project):
        """§13/E54: merge only allow-listed selectors and provider configuration in memory."""
        query=urllib.parse.urlencode({'location[directory]':str(project)})
        status,raw=self._request('GET','/api/config?'+query,
            timeout=max(.001,self._observation_deadline-time.monotonic()))
        value=self._json(raw)
        entries=value.get('data') if type(value) is dict else value
        if status!=200 or type(entries) is not list:
            raise Blocked('effective configuration sources unobservable')
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
        return effective,agents,providers

    @reply_check
    def _auxiliary_bindings(self,identity):
        schema=self._served_schema()
        offered=schema.get('components',{}).get('schemas',{}).get('Config.InfoEncoded',{}).get('properties',{})
        if not {'model','agents','providers'}<=set(offered):
            raise Blocked('pinned effective selector readback schema unsupported')
        project=Path(self.project);reload=self._endpoint_reloads.get(project)
        expected=self._endpoint_map(self.provider_endpoints)
        def selector(model):
            if type(model) is str: return model.split('#',1)[0]
            if type(model) is dict and type(model.get('providerID')) is str and type(model.get('model')) is str:
                return model['providerID']+'/'+model['model']
            raise Blocked('effective model selector unknown')
        def read():
            effective,agents,providers=self._effective_configuration(project)
            endpoints=self._validate_provider_config({'providers':providers})
            observed=self._endpoint_map(endpoints)
            bindings=[]
            if 'model' in effective: bindings.append(selector(effective['model']))
            for row in agents.values():
                if row.get('disabled') is not True and 'model' in row:
                    bindings.append(selector(row['model']))
            if any(model!=identity for model in bindings):
                self.guard.stopped=True; raise Blocked('auxiliary model drift or paid identity')
            required={'via','title','summary','compaction','explore','general','build','plan'}
            complete='model' in effective and required<=set(agents) and all(
                row.get('disabled') is True or 'model' in row for row in agents.values())
            if observed==expected and endpoints==self.provider_endpoints and complete:
                return bindings
            if reload is not None and reload['expected']==expected:
                previous=reload['previous']
                if not observed:
                    return None
                if previous is not None and observed==previous:
                    if reload['identity']!=self._config_identity():
                        raise Blocked('effective configuration previous generation changed')
                    return None
            if observed!=expected or endpoints!=self.provider_endpoints:
                raise Blocked('effective provider endpoints differ from owned fixture')
            if 'model' not in effective: raise Blocked('effective default model unavailable')
            if not required<=set(agents): raise Blocked('effective auxiliary agent selectors incomplete')
            raise Blocked('effective auxiliary model is not frozen')
        bindings=self._await_owned(read,'owned effective configuration readiness deadline',
                                   interval=CONFIG_READY_POLL_SECONDS)
        self._verified_endpoint_maps[project]={'identity':self._config_identity(),'endpoints':expected}
        self._endpoint_reloads.pop(project,None)
        return bindings

    @reply_check
    def _served_schema(self):
        """Verify §2 E7 before interpreting vendor-specific qualification routes."""
        if getattr(self,'_schema_generation',None)==self.vendor_identity:
            return self._pinned_schema
        status,raw=self._request('GET','/openapi.json')
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
                self._timeline_bind(session,reply['vendor_session_id'])
                return reply['vendor_session_id']
        if session not in self.handles: raise Blocked('confirmed vendor session ID missing')
        def read():
            value=self.via(['status',session])
            sid=value.get('vendor_session_id')
            if type(sid) is str:
                if value.get('vendor_identity_verified') is not True:
                    raise Blocked('native session identity unverified')
                self._timeline_bind(session,sid)
                return sid
            if sid is not None and type(sid) is not str:
                raise Blocked('native session identity malformed')
            # C1 §§1,3.7: status.state is the session state, not a turn state.
            if (value.get('state')!='active' or value.get('admission')!='open') \
                    and not self._queued_status(value,session):
                raise Blocked('session ended before native creation')
            return None
        return self._await_owned(read,'confirmed vendor session ID missing')

    @reply_check
    def start_event_capture(self):
        """Capture bounded owned SSE in memory for real request/action attribution."""
        self.close_event_capture()
        self._event_stop.clear(); self.events=[]; self.events_error=None; self._event_failure=None
        parts=urllib.parse.urlsplit(self._http.origin)
        conn=http.client.HTTPConnection(parts.hostname,parts.port,timeout=1)
        self._event_conn=conn
        password=self._http.password
        auth=base64.b64encode(b'opencode:'+password).decode()
        self._verify(self.vendor_identity)
        conn.connect(); self._event_socket=conn.sock
        ready=threading.Event(); header_deadline=time.monotonic()+SSE_HANDSHAKE_SECONDS
        stream_deadline=self.phase_deadline or time.monotonic()+120*60
        def watchdog():
            while not self._event_stop.wait(.01):
                deadline=(self.phase_deadline or stream_deadline) if ready.is_set() else header_deadline
                if time.monotonic()>=deadline:
                    self._stash_event_failure(Blocked('owned event stream deadline'),'Deadline'); break
            if self._event_socket:
                try: self._event_socket.shutdown(socket.SHUT_RDWR)
                except OSError: pass
        self._event_watchdog=threading.Thread(target=watchdog,name='owned-opencode-sse-deadline',daemon=True)
        self._event_watchdog.start()
        try:
            conn.request('GET','/api/event',headers={'Authorization':'Basic '+auth,'Accept':'text/event-stream'})
            response=conn.getresponse()
            self.reply_evidence.capture('vendor-sse-handshake','/api/event',b'',
                http_status=response.status,body_read=False)
            # Normal idle SSE has no per-read timeout. The owned watchdog enforces
            # the absolute phase ceiling and shuts this saved socket down on stop.
            self._event_socket.settimeout(None)
            ready.set()
        except BaseException as error:
            self.close_event_capture()
            raise Blocked('owned event handshake failed within absolute deadline') from error
        if response.status!=200:
            response.close(); self.close_event_capture(); raise Blocked('owned native event stream unavailable')
        generation=self.vendor_identity
        def consume():
            current=bytearray(); data=[]; event_id=None; total=0
            try:
                while not self._event_stop.is_set():
                    line=response.readline(1024*1024+1)
                    if not line: raise Blocked('owned event stream ended')
                    total+=len(line); current.extend(line)
                    if len(current)>1024*1024 or total>OBSERVATION_BYTES:
                        self.reply_evidence.capture('vendor-sse','/api/event',bytes(current),http_status=200)
                        raise Blocked('owned event capture bound')
                    if line in (b'\n',b'\r\n'):
                        if data:
                            raw=b'\n'.join(data)
                            self.reply_evidence.capture('vendor-sse','/api/event',raw,http_status=200)
                            if self.vault.leaks(raw): raise Blocked('password in event stream')
                            value=self._json(raw)
                            if type(value) is not dict or type(value.get('type')) is not str:
                                raise Blocked('native event shape unavailable')
                            with self._events_lock:
                                event={'seq':len(self.events)+1,'vendor_event_id':event_id,
                                       **value,'_observed_at':time.monotonic()}
                                self.events.append(event)
                            self._timeline_event(event,generation)
                        current.clear(); data=[]; event_id=None
                    elif line.startswith(b'data:'): data.append(line[5:].lstrip().rstrip(b'\r\n'))
                    elif line.startswith(b'id:'): event_id=line[3:].strip().decode('utf-8','strict')
            except BaseException as error:
                if not self._event_stop.is_set():
                    self._stash_event_failure(error,type(error).__name__)
            finally: response.close(); conn.close()
        self._event_thread=threading.Thread(target=consume,name='owned-opencode-events',daemon=True)
        self._event_thread.start()

    def _stash_event_failure(self,error,kind):
        """§13: observer threads only store sanitized context in memory."""
        with self._events_lock:
            if self._event_failure is None:
                self._event_failure=(error,self.reply_evidence.snapshot(
                    {'vendor-sse','vendor-sse-handshake'}))
                self.events_error=kind

    def _retain_event_failure(self):
        """§13: evidence files and ownership entries are main-thread work."""
        with self._events_lock: failure=self._event_failure
        if failure is not None:
            error,replies=failure
            self.reply_evidence.block(error,replies)
            return error

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
        self._retain_event_failure()

    @reply_check
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
                active+=status['state']=='active'
            return {'complete':True,'rss_bytes':rss,'at':time.monotonic(),
                    'active_sessions':active,'sessions':len(self._l9_sessions),
                    'retained_keys':None,'retained_bytes':None,'within_packet_bounds':None,
                    'retained_bounds_proof':None,'vendor_identity':self.vendor_identity.report()}
        if kind=='native_events':
            if type(target) is dict: target=target.get('session')
            if self.events_error:
                cause=self._retain_event_failure()
                error=Blocked('native stream observation failed')
                if cause is not None:
                    error.reply_evidence=getattr(cause,'reply_evidence',[])
                    if getattr(cause,'reply_retention_failed',False): error.reply_retention_failed=True
                raise error from cause
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
            self.ensure_vendor(); status,_=self._request('GET','/api/info',authenticated=False)
            wrong=safety.OwnedHTTP(self._http.origin,self.vendor_identity,self.proc,b'wrong-owned-fixture')
            wrong_status,_=self._request('GET','/api/info',client=wrong)
            good,raw=self._request('GET','/api/info'); info=self._json(raw)
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
            expected={name for name in ('mcp','plugin','hook')
                      if (self.project/'.opencode'/(name+'-helper.py')).is_file()}
            if (self.project/'.opencode'/'lsp-helper.py').is_file() \
                    and getattr(self,'_lsp_result',{}).get('spawned') is True: expected.add('lsp')
            def read():
                values=self._helper_snapshot()
                return {'complete':True,'helpers':values} if values and expected<=set(
                    value.get('kind') for value in values) else None
            # Marker loss inspects the captured helper ledger after vendor death;
            # it cannot use a readiness wait requiring that dead generation alive.
            if self.vendor_identity is not None and self.proc.alive(self.vendor_identity) is False:
                result=read()
                if result is None: raise Blocked('helper observations missing')
                return result
            return self._await_owned(read,'helper observations missing')
        if kind=='marker_helper':
            # Host's marker-matched leftover report is public outcome evidence, not /proc search authority.
            helpers=self.observe('helper_observations')['helpers']
            reports=[reply['leftovers'] for reply in self.owned_replies if type(reply.get('leftovers')) is dict]
            if not reports: raise Blocked('Host leftover report unavailable')
            marker=[row for row in helpers if row.get('kind')=='marker']
            if len(marker)!=1: raise Blocked('marker helper identity ambiguous')
            helper=marker[0]; identity=safety.Identity(helper['pid'],helper['start_ticks'])
            self._verify(identity)
            started=self._started_at(identity.start_ticks)
            report=reports[-1]
            _typed(report,{'scope':S,'processes':[{'pid':I,'started_at':S}],'incomplete':B},'Host leftovers')
            matches=any(row['pid']==identity.pid and row['started_at']==started for row in report['processes'])
            self._record('marker-leftover-proof',{'helper_identity_listed':matches,
                'report_incomplete':report['incomplete'],'listed_processes':len(report['processes']),
                'report_scope':report['scope']})
            # C1 §5: incomplete qualifies the full set, not a positively observed entry.
            if not matches: raise Blocked('Host exact-marker helper proof unavailable')
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

    def _helper_snapshot(self):
        """§13: one metadata/identity checked helper snapshot, no invented spawn facts."""
        folder=self._helper_folder(self.project)
        if not folder.is_dir(): raise Blocked('helper observation absent')
        values=[]
        for path in sorted(folder.glob('*.json')):
            if path.stat().st_size>OBSERVATION_BYTES: raise Blocked('helper observation bound')
            raw=path.read_bytes()
            if self.vault.leaks(raw): raise Blocked('helper wrote password')
            value=self._json(raw)
            failure=self._helper_failure(value)
            if failure is not None:
                self._record('helper-failure',failure)
                raise Blocked('helper reported failure')
            _typed(value,{'pid':I,'start_ticks':I,'password_key_absent':B},'helper')
            identity=safety.Identity(value['pid'],value['start_ticks'])
            if self.proc.alive(identity) is True:
                self._verify(identity)
                if identity in self.helper_ledger and self.helper_generations.get(identity)!=self.vendor_identity:
                    raise Blocked('helper readiness generation changed')
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
        return values

    @staticmethod
    def _event_session(event):
        value=event.get('data',event)
        if type(value) is not dict: return None
        if isinstance(value.get('sessionID'),str): return value['sessionID']
        form=value.get('form')
        return form.get('sessionID') if type(form) is dict else None

    def secrecy_scan(self):
        """Restart the whole registered-root scan on VIA churn, at most 3 times (§13)."""
        leak_seen=False
        def record_leak():
            nonlocal leak_seen
            leak_seen=True
        for attempt in range(1,SCAN_ATTEMPTS+1):
            try:
                result=self._secrecy_scan_once(record_leak)
                result['secret_absent']=result['secret_absent'] and not leak_seen
                result['scan_attempts']=attempt
                return result
            except _ScanVanished as error:
                if attempt==SCAN_ATTEMPTS: raise Blocked('VIA scan entry vanished after 3 scans') from error

    def _secrecy_scan_once(self,record_leak):
        """Scan immutable ownership history; Store backups cover every state (§13 L4/L11)."""
        clean=True; captures=[]; seen=set(); metadata=set(); synthetic=[]; total=0
        vendor_synthetic=0; vanished=set()
        handles={handle.encode() for handle in self.handles.values()} | self.bearer_forms
        synthetic_forms=[form for form in self.secret_forms if form and form not in handles]
        fixture_sources={row['path']/'opencode.json' for row in self.fixtures.values()}
        states=[root for root in self.ownership.ordered() if root.state]
        store_files={path for root in states for path in (
            root.path/'store.sqlite3',root.path/'store.sqlite3-wal',root.path/'store.sqlite3-shm')}
        databases=[]; checked_roots=set()
        def remember(path):
            seen.add(path)
            if len(seen)>SCAN_ENTRIES: raise Blocked('private secrecy scan entry bound')
        def unavailable(path,error):
            root=self.ownership.classify(path)
            if isinstance(error,FileNotFoundError) or isinstance(error,OSError) and error.errno in {errno.ENOENT,errno.ESRCH}:
                if root is not None and root.kind=='vendor-private':
                    remember(path); vanished.add(path); return
                raise _ScanVanished() from error
            raise Blocked('private secrecy scan entry unreadable') from error
        def nonregular(path,mode):
            root=self.ownership.classify(path)
            if root is not None and root.kind=='vendor-private' or self.ownership.allowed_nonregular(path,mode):
                metadata.add(path); return
            raise Blocked('non-regular VIA scan entry')
        def protected(raw,via_sink):
            matched=self.vault.leaks(raw) or any(handle in raw for handle in handles) \
                or via_sink and any(form in raw for form in synthetic_forms)
            if matched: record_leak()
            return matched
        for root in self.ownership.ordered():
            if not root.directory or root.kind=='vendor-private': continue
            try: info=root.path.lstat()
            except FileNotFoundError:
                if root.path==self.evidence or root.state and root.started:
                    raise Blocked('VIA scan root missing')
                continue
            except OSError as error: raise Blocked('VIA scan root unreadable') from error
            if not stat.S_ISDIR(info.st_mode): raise Blocked('VIA scan root is not a directory')
            checked_roots.add(root.path)
        for state_root in states:
            db=state_root.path/'store.sqlite3'
            for path in (db,Path(str(db)+'-wal'),Path(str(db)+'-shm')):
                try: info=path.lstat()
                except FileNotFoundError:
                    if path==db and state_root.started: raise Blocked('VIA Store missing after daemon run')
                    continue
                except OSError as error: raise Blocked('VIA Store unreadable') from error
                if not stat.S_ISREG(info.st_mode): raise Blocked('VIA Store is not a regular file')
                if path==db: databases.append(path)
        for walk_root in self.ownership.walks():
            try: info=walk_root.lstat()
            except FileNotFoundError:
                if walk_root in checked_roots: unavailable(walk_root,FileNotFoundError())
                continue
            except OSError as error: unavailable(walk_root,error); continue
            if not stat.S_ISDIR(info.st_mode):
                remember(walk_root); nonregular(walk_root,info.st_mode); continue
            def unreadable(error):
                if error.filename is None: raise Blocked('private secrecy scan directory unreadable') from error
                unavailable(Path(error.filename),error)
            for directory,dirs,files in os.walk(walk_root,followlinks=False,onerror=unreadable):
                dirs.sort(); files.sort()
                for name in dirs[:]:
                    path=Path(directory)/name; root=self.ownership.classify(path)
                    if name=='.git' and root is not None and root.kind=='vendor-private':
                        dirs.remove(name); continue
                    remember(path)
                    try: info=path.lstat()
                    except OSError as error: dirs.remove(name); unavailable(path,error); continue
                    if not stat.S_ISDIR(info.st_mode):
                        dirs.remove(name); nonregular(path,info.st_mode)
                for name in files:
                    path=Path(directory)/name; root=self.ownership.classify(path)
                    remember(path)
                    try: info=path.lstat()
                    except OSError as error: unavailable(path,error); continue
                    if not stat.S_ISREG(info.st_mode): nonregular(path,info.st_mode); continue
                    if root is None: raise Blocked('unregistered evidence file')
                    if path in store_files: continue  # Every state's consistent backup below.
                    via_sink=root.kind in {'via-owned','runner-evidence'}
                    source=path in fixture_sources
                    parts=path.relative_to(root.path).parts
                    protected_name=any(safety.credential_metadata_only(part) for part in parts) \
                        or name.lower().endswith('.db') or name=='opencode.json' and not source
                    if protected_name:
                        if via_sink:
                            for part in parts:
                                lowered=part.lower().lstrip('.')
                                if lowered.startswith(('auth','npmrc','netrc','bunfig.toml')) or 'credential' in lowered:
                                    raise Blocked('protected content in VIA-owned scan')
                        else: metadata.add(path); continue
                    readable=via_sink or source or path.suffix.lower() in {'.json','.jsonl','.ndjson','.log','.txt'} \
                        or any(part in {'log','logs'} for part in parts) \
                        or any(token in name.lower() for token in ('stderr','stdout','output','undecoded'))
                    if not readable: continue
                    if info.st_size>OBSERVATION_BYTES: raise Blocked('evidence scan bound')
                    try:
                        with path.open('rb') as file: raw=file.read(OBSERVATION_BYTES+1)
                    except OSError as error: unavailable(path,error); continue
                    if len(raw)>OBSERVATION_BYTES: raise Blocked('evidence scan bound')
                    total+=len(raw)
                    if total>EVIDENCE_BYTES: raise Blocked('private secrecy scan byte bound')
                    clean=clean and not protected(raw,via_sink)
                    if source: synthetic.append(path)
                    elif root.kind=='vendor-private' and any(form in raw for form in synthetic_forms):
                        vendor_synthetic+=1
                    if via_sink and path.name in {'undecoded.bin','stderr.log'}: captures.append(path.name)
        for db in databases:
            try:
                info=db.lstat()
                if not stat.S_ISREG(info.st_mode): raise Blocked('VIA Store is not a regular file')
                if info.st_size>EVIDENCE_BYTES: raise Blocked('Store scan bound')
                with sqlite3.connect(f'file:{db}?mode=ro',uri=True) as source:
                    with sqlite3.connect(':memory:') as destination:
                        source.backup(destination)
                        raw=destination.serialize()
            except FileNotFoundError as error: unavailable(db,error)
            except (OSError,sqlite3.Error) as error: raise Blocked('VIA Store backup unverifiable') from error
            total+=len(raw)
            if total>EVIDENCE_BYTES: raise Blocked('private secrecy scan byte bound')
            clean=clean and not protected(raw,True)
        return {'complete':True,'secret_absent':clean,'payload_captures':len(captures),
                'metadata_only_files':len(metadata),'synthetic_source_files':len(synthetic),
                'vendor_private_synthetic_files':vendor_synthetic,
                'vendor_private_vanished_entries':len(vanished),'store_backups':len(databases),
                'scope':['all-registered-states','all-registered-vendor-roots','registered-evidence','daemon-home-xdg-tmp'],
                'synthetic_gate_scope':'via-owned-sinks',
                'exclusions':['vendor-auth-config-and-database-content',
                              'synthetic-provider-values-in-private-fixture-config','registered-helper-artifacts']}

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

    @reply_check
    def seam_wait(self,name,*,timeout=30):
        if name not in self._armed: raise Blocked('seam was not armed')
        occurrence=self._armed[name]
        path=Path(self.env['VIA_FAILPOINT_DIR'])/f'{name}.{occurrence}.ack'
        def read():
            try: raw=path.read_bytes()
            except FileNotFoundError: return None
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
        return self._await_owned(read,'seam entry unobserved',seconds=timeout)

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
            identity=None
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
                            self._verify(identity)
                            continue
                        raise
                    if any(lockid.encode() in line and b'FLOCK' in line for line in raw.splitlines()):
                        holders.append(identity.report())
            except (OSError,Blocked) as error:
                # A present stat can describe an exited process. Only pidfd-
                # grounded False permits skipping; hidden/live directories block.
                alive=self.proc.alive(identity) if identity is not None else None
                if alive is False:continue
                role=self._process_role(identity) if identity is not None else 'host'
                self._record('lock-scan-block',{'pid':int(directory.name),
                    'start_ticks':identity.start_ticks if identity else None,
                    'role':role,'liveness':alive if type(alive) is bool else None,
                    'os_error':safety.os_error_record(error)})
                raise safety.process_block(role,alive,'same-uid lock descriptor scan unverifiable') from error
        unique={tuple(sorted(row.items())) for row in holders}
        holders=[dict(row) for row in unique]
        if holders!=[anchor.report()]: raise Blocked('namespace lock escaped anchor')
        self._verify(anchor,'anchor')
        return holders

    @reply_check
    def lifecycle(self,point):
        """Capture Host identity/lock facts for an explicitly reached life-point barrier."""
        self._prepare_lifecycle(point)
        self.ensure_vendor(); record=self._host_record()
        self._verify(self.daemon); self._verify(self.anchor); self._verify(self.vendor_identity)
        status,raw=self._request('GET','/api/info'); info=self._json(raw)
        if status!=200 or info['pid']!=self.vendor_identity.pid: raise Blocked('vendor PID drift')
        if not self.last_request or 'session_id' not in self.last_request['receipt']:
            raise Blocked('L14 requires an active Host-backed VIA turn pin')
        active_session=self.last_request['receipt']['session_id']
        status_reply=self._live_pin(active_session)
        active=status_reply.get('active_turn')
        live_pin=type(active) is dict and active.get('phase')=='accepted' \
            and status_reply.get('state')=='active' \
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
            self._verify(safety.Identity(**sample['helper']))
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
            verified=threading.Event(); generation=[]
            def publication_request(model):
                self._mock_request_admit(model); self._publication_entered.set()
                deadline=min(self.phase_deadline or float('inf'),time.monotonic()+180)
                while not self._publication_release.wait(.01):
                    if self.signals: self.signals.guard()
                    if time.monotonic()>=deadline: raise Blocked('publication response barrier deadline')
                    # The request can arrive before ensure_vendor finishes.
                    # Publish only that completed observation, never a stale
                    # field left over from the retired predecessor.
                    if not verified.is_set(): continue
                    identity=generation[0]
                    alive=self.proc.alive(identity)
                    if alive is False: return  # L14 intentionally killed this generation.
                    if alive is None: raise safety.process_block('vendor',None,'publication vendor identity unverifiable')
                    if self.vendor_identity!=identity:
                        raise Blocked('publication vendor generation changed')
                    try: self._verify(identity)
                    except Blocked:
                        if self.proc.gone(identity): return
                        raise
            provider.admit_request=publication_request
        receipt=self.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                          '--cwd',str(project),'--bound','full','--network','--background',
                          '--prompt','Run the local qualification tool helper and hold its barrier.'])
        if 'cli_error' in receipt: raise Blocked('fresh L14 turn refused')
        self.ensure_vendor()
        if point=='publication':
            if self.vendor_identity is None: raise Blocked('publication verified vendor identity absent')
            generation.append(self.vendor_identity); verified.set()
            self._await_owned(lambda:True if self._publication_entered.is_set() else None,
                              'publication model request unobserved')
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
            self._await_helper_point('tool','tool_after')
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
                self._await_helper_point(kind,point)
                return
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
                    try:
                        if os.readlink(entry,dir_fd=fds)!=pipe: continue
                    except OSError as error:
                        if error.errno not in {errno.ENOENT,errno.ESRCH}:raise
                        self._verify(daemon,'daemon');continue
                    name='fdinfo/'+entry
                    try:raw=self.proc.read(daemon,name)
                    except Blocked as error:
                        cause=error.__cause__
                        if not isinstance(cause,OSError) or cause.errno not in {errno.ENOENT,errno.ESRCH} \
                                or cause.filename!=name:raise
                        self._verify(daemon,'daemon');continue
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
        for identity in identities.values(): self._verify(identity)
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

    def _prepare_l11_mock_provider(self):
        """§13 L11: register a known integration persistently, with model admission closed."""
        config=self._materialize_fixture('l11-seed',self.namespace,{'provider':'mock'})
        self._validate_provider_config(config)
        self._write_fixture_config(self.namespace,config)
        self._refresh_inventory()

    def _direct_seed_info(self,http,identity,deadline):
        """§13 L11: only matching owned startup 503 is pending; ambiguity blocks."""
        while True:
            if self.signals: self.signals.guard()
            self._verify(identity)
            if time.monotonic()>=deadline: raise Blocked('L11 direct info startup deadline')
            status,raw=self._request('GET','/api/info',client=http)
            info=self._json(raw)
            if type(info) is not dict or info.get('pid')!=identity.pid \
                    or info.get('version')!='2.0.22' or status not in {200,503}:
                raise Blocked('L11 direct identity/version mismatch')
            if status==200: return
            time.sleep(min(.02,max(0,deadline-time.monotonic())))

    @reply_check
    def direct_seed(self,namespace,mutation):
        """Direct credential-seeding server only while private VIA stopped (§13 L11)."""
        self._check_ancestors('direct-seed')
        if self.daemon is not None or Path(namespace)!=self.namespace:
            raise Blocked('L11 requires exact stopped VIA namespace')
        if not self._locks_free(): raise Blocked('L11 private VIA locks not free')
        safety.verify_binary(self.pinned)
        if mutation=={'schema_checked':True}: self._prepare_l11_mock_provider()
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
            deadline=min(time.monotonic()+30,self.phase_deadline or float('inf'))
            while True:
                if self.signals: self.signals.guard()
                self._verify(identity)
                if time.monotonic()>=deadline: raise Blocked('L11 direct listener startup deadline')
                try: origin=self.proc.listener(identity); break
                except safety.ListenerNotReady:
                    time.sleep(min(.01,max(0,deadline-time.monotonic())))
            http=safety.OwnedHTTP(origin,identity,self.proc,password,seeding_mode=True,deadline=deadline)
            self._direct_seed_info(http,identity,deadline)
            status,raw=self._request('GET','/openapi.json',client=http)
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
                    'integrationID':safety.MOCK_IDENTITY.split('/')[0],'label':'VIA synthetic fixture',
                    'value':{'type':'key','key':secret},'activate':True}}
            self._validate_seed_mutation(schema,mutation)
            status,raw=self._request(mutation['method'],mutation['path'],mutation['body'],client=http)
            if status not in {200,201,204}: raise Blocked('L11 synthetic seeding refused')
            status,raw=self._request('GET','/api/integration',client=http)
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
                if identity: self._verify(identity); process.kill()
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
        rc,out,_=self.execute(['/usr/bin/pgrep','-f','--',str(self.work)],env=self.env,timeout=10)
        if rc not in {0,1}: return None
        if rc==1: return True
        try: pids={int(line) for line in out.splitlines()}
        except ValueError: return None
        # pgrep excludes itself. The runner may name evidence in argv and is outside owned subtree.
        pids.discard(os.getpid())
        return not pids

    def stop(self):
        """Stop all observed owned generations, then prove locks and identities absent."""
        self.stop_result=None
        if self.journal.path.exists(): self.journal.recover()
        for provider in self.mock_providers.values():
            if provider.response_hold is not None: provider.response_hold.abort()
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
        close_error=None
        if self.daemon is not None:
            alive=self.proc.alive(self.daemon)
            if alive is None: raise safety.process_block('daemon',None,'private daemon stop identity uncertain')
            if alive:
                self._verify(self.daemon)
                # §13: close the acquisition lease even after admission expires.
                if self._bootstrap_session is not None:
                    self._bootstrap_cleanup=True
                    try:
                        closed=self.via(['close',self._bootstrap_session])
                        if closed.get('state')!='closed': raise Blocked('bootstrap cleanup close refused')
                        self._bootstrap_session=None
                    except BaseException as error:
                        close_error=error
                    finally:
                        self._bootstrap_cleanup=False
                # A close reply can prove this generation's clean idle end.
                if self.daemon is not None:
                    reply=self.via(['daemon','stop','--force'])
                    if reply.get('exit_code'): raise Blocked('private daemon stop refused')
        deadline=time.monotonic()+CLEANUP_SECONDS
        while time.monotonic()<deadline:
            locks=[self._lock_free(path) for path in self._lock_paths()]
            if any(lock is False for lock in locks):
                self._discover_private_lockers(strict=True)
            owned=sorted(self.identities,key=lambda value:(value.pid,value.start_ticks))
            states=[self.proc.alive(identity) for identity in owned]
            locks=[self._lock_free(path) for path in self._lock_paths()]
            if any(state is None for state in states):
                # Retain the identity corresponding to the original observation,
                # rather than sampling again during cleanup diagnostics.
                identity=owned[states.index(None)]
                raise safety.process_block(self._process_role(identity),None,
                                           'owned stop identity or lock unverifiable')
            if any(lock is None for lock in locks):
                raise Blocked('owned stop identity or lock unverifiable')
            if all(state is False for state in states) and locks==[True,True]: break
            time.sleep(0.2)
        pgrep=self._pgrep_clear()
        safety.stop_proof(process_states=states,locks_free=locks,uncertain=self.uncertain,pgrep_clear=pgrep)
        self.stop_result={'complete':True,'proven':True,'processes_gone':True,'locks_free':True,'pgrep_clear':True}
        self._record('daemon-stop',self.stop_result)
        self.daemon=None; self.anchor=None; self._http=None
        self._bootstrap_session=None
        self._armed={}; self._armed_history=set(); self._seam_targets={}; self._event_generation=None
        if self.runtime.parent==Path('/tmp') and self.runtime.name.startswith('via-ocl.'):
            shutil.rmtree(self.runtime)
        if self.pinned.exists(): safety.verify_binary(self.pinned)
        if close_error is not None: raise close_error
        return self.stop_result

    def _materialize_fixture(self,name,project,description):
        """Create reviewed local mock/helpers from case descriptors, never install packages."""
        from opencode_cases import LoopbackProvider, HOSTILE_RESPONSES
        config={key:value for key,value in description.items() if key in
                {'providers','model','agents','permissions','lsp','mcp','plugins','instructions','compaction'}}
        descriptors={'provider','response','hostile_keys','context','helper','separate_group','fake_lsp',
                     'subagent','subagent_permission','skills','instructions','large_fields','script','mcp','plugin','hook','cache_read','cache_write','script_after_setup','response_hold'}
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
                                      cache_write=description.get('cache_write',3),response_hold=description.get('response_hold'),
                                      retain_exchanges=name=='usage-cache')
            if description.get('large_fields'):
                provider.text=('VIA escape "\\\\\\n\\t雪 '*4096)
            if description.get('hostile_keys') and description.get('response') in HOSTILE_RESPONSES:
                from opencode_mock_policy import hostile_budget
                budget=hostile_budget(description['response'])
                self._mock_provider_left[name]=budget['requests']
                self._record('mock-provider-budget',{'fixture':name,**budget})
            provider.admit_request=lambda model:self._mock_request_admit(model,fixture=name)
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
            plugins=self._directory(project/'.opencode'/'plugins','vendor-private')
            directory=self._directory(plugins/'via-fixture','vendor-private')
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
            if name=='never-ask':
                # §13 L5/§11: ask on an actual child action; only interactive child events reach C1.
                code+="  await ctx.permission.hook('evaluate', async event => {\n" \
                      +"    if (event.agent==='oclive-ask' && event.action==='shell' && event.source?.type==='tool') {\n" \
                      +"      event.effect='ask'; event.message='VIA private fixture ask';\n" \
                      +"    }\n  });\n"
            code+="  return () => { for (const child of children) { if (child.exitCode===null) child.kill('SIGTERM'); } };\n }\n};\n"
            (directory/'index.js').write_text(code); (directory/'index.js').chmod(0o400)
            config['plugins']=[str(directory)]
        if description.get('skills')=='off':
            skill=self.ownership.register(project/'.claude'/'skills'/'private-fixture','vendor-private')
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
        observations=self._directory(project/'.opencode'/'observations','vendor-private')
        self.helper_channels[str(project)]=observations
        path=project/'.opencode'/f'{kind}-helper.py'
        code='''#!/usr/bin/python3
import errno, json, os, pathlib, time, sys, threading
kind=KIND
if kind=='marker':
    step='fork'
    pid=os.fork()
    if pid:
        step='barrier'
        end=time.monotonic()+BARRIER_SECONDS
        while not (pathlib.Path(OBSERVATIONS)/'marker-parent.release').exists() and time.monotonic()<end:
            time.sleep(.01)
        sys.exit(0)
# §13: the pinned shell may exec this helper as an already detached session leader.
step='session'
if SEPARATE and os.getsid(0)!=os.getpid():
    os.setsid()
root=pathlib.Path(OBSERVATIONS)
step='identity'
raw=pathlib.Path('/proc/self/stat').read_bytes()
ticks=int(raw[raw.rindex(b')')+2:].split()[19])
value={'pid':os.getpid(),'start_ticks':ticks,'kind':kind,'point':kind+'_during',
       'ready':True,'password_key_absent':'OPENCODE_PASSWORD' not in os.environ,
       'pgid':os.getpgrp()}
path=root/(kind+'.json')
def persist():
    with report_lock:
        if failure_seen.is_set(): return
        temporary=root/(kind+'.'+str(os.getpid())+'.tmp')
        temporary.write_text(json.dumps(value)); temporary.chmod(0o600)
        temporary.replace(path)
step='readiness'
persist()
if kind in {'lsp','mcp'}:
    def released():
        try:
            end=time.monotonic()+BARRIER_SECONDS
            while not (root/(kind+'.release')).exists() and time.monotonic()<end:
                time.sleep(.01)
            value['point']=kind+'_after'
            persist()
        except BaseException as error:
            fail_closed(error,'release')
    threading.Thread(target=released,daemon=True).start()
if kind=='mcp':
    step='protocol'
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
    step='protocol'
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
    step='barrier'
    end=time.monotonic()+BARRIER_SECONDS
    while not (root/(kind+'.release')).exists() and time.monotonic()<end:
        time.sleep(.01)
value['point']=kind+'_after'; value['ready']=True
step='complete'
persist()
if not failure_seen.is_set(): print('VIA HELPER DONE')
'''
        header,body=code.split('kind=KIND',1)
        reporter='''kind=KIND
root=pathlib.Path(OBSERVATIONS)
step='initialization'
report_lock=threading.Lock()
failure_seen=threading.Event()
def record_failure(error,reached):
    name=type(error).__name__
    number=getattr(error,'errno',None)
    failure={'kind':kind,'ready':False,'failure':{
        'exception_class':name if name in EXCEPTION_CLASSES else 'other',
        'errno':errno.errorcode.get(number) if type(number) is int else None,
        'step':reached if reached in FAILURE_STEPS else 'other'}}
    with report_lock:
        failure_seen.set()
        temporary=root/(kind+'.'+str(os.getpid())+'.failure.tmp')
        temporary.write_text(json.dumps(failure)); temporary.chmod(0o600)
        temporary.replace(root/(kind+'.json'))
def fail_closed(error,reached):
    try:
        record_failure(error,reached)
    finally:
        try: os.write(2,b'VIA fixture helper failed\\n')
        finally: os._exit(1)
'''
        handler='''except BaseException as error:
    if isinstance(error,SystemExit) and error.code in (None,0): raise
    fail_closed(error,step)
'''
        code=header+reporter+'try:\n'+textwrap.indent('kind=KIND'+body,'    ')+handler
        code=code.replace('EXCEPTION_CLASSES',repr(tuple(sorted(safety.HELPER_EXCEPTION_CLASSES))))
        code=code.replace('FAILURE_STEPS',repr(tuple(sorted(safety.HELPER_FAILURE_STEPS))))
        code=code.replace('KIND',repr(kind)).replace('SEPARATE',repr(separate_group)).replace('OBSERVATIONS',repr(str(observations)))
        code=code.replace('BARRIER_SECONDS',str(barrier_seconds))
        path.write_text(code); path.chmod(0o500)
        return path

    def _helper_failure(self,row):
        """§13: failed-helper data is diagnostic only; it cannot prove an owned spawn."""
        if type(row) is not dict or row.get('ready') is not False or 'failure' not in row:return None
        failure=row['failure']
        if type(failure) is not dict:raise Blocked('helper failure record malformed')
        def label(value,allowed):
            return value if type(value) is str and value in allowed \
                and not self._reply_protected(value.encode()) else 'other'
        number=label(failure.get('errno'),frozenset(errno.errorcode.values()))
        return {'ready':False,
            'exception_class':label(failure.get('exception_class'),safety.HELPER_EXCEPTION_CLASSES),
            'errno':None if number=='other' else number,
            'step':label(failure.get('step'),safety.HELPER_FAILURE_STEPS)}

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
                    self._verify(identity)
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
            self._verify(self.daemon)
            row=self.proc.stat(self.daemon.pid)
            if not row or row['start_ticks']!=self.daemon.start_ticks or row['state'] not in {'T','t'}:
                raise Blocked('daemon barrier not held')
            self._verify(identity)
            self._sigkill_at=time.monotonic(); self.journal._signal(identity,signal.SIGKILL,role='anchor')
            self._killed_anchor=identity
            return {'pid_only':True,'monotonic':float(self._sigkill_at)}
        if operation=='vendor_death':
            if identity!=self.vendor_identity or self._sigkill_at is None:
                raise Blocked('vendor death sample not owned')
            deadline=sample['deadline']
            if deadline>self._sigkill_at+1.000001: raise Blocked('death window exceeds one second')
            while time.monotonic()<=deadline:
                self._verify(self.daemon)
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
                self._verify(identity)
                row=self.proc.stat(identity.pid)
                if row and row['start_ticks']==identity.start_ticks and row['state'] not in {'T','t'}:
                    return {'identity_verified':True}
                time.sleep(.005)
            raise Blocked('owned daemon continuation unverified')
        raise Blocked('unknown L14 barrier operation')

    @reply_check
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
            self._live_pin(session)
            if not self.vendor_identity: raise Blocked('marker vendor identity absent')
            # §10: Host confirms the vendor's exit through its still-live anchor.
            # Killing the anchor instead loses that control and can yield unknown.
            self._verify(self.vendor_identity)
            self.journal._signal(self.vendor_identity,signal.SIGKILL,role='vendor')
            return {'vendor_pid_only':True}
        if kind=='helper_stop_proof':
            helper=args['helper']
            rows=[(identity,row) for identity,row in self.helper_ledger.items() if row.get('kind')==helper]
            if not rows: raise Blocked('helper identity was never captured')
            self._await_gone([identity for identity,_ in rows],'helper stop/pgrep absence unverified')
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
            self.state=self.ownership.register_state(self.work/'l11-state')
            safety.private_directory(self.state)
            self.namespace,self.namespace_env=self._create_namespace()
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
            provider=next((p for name,p in self.mock_providers.items()
                           if self.fixtures.get(name,{}).get('path')==self.project),None)
            if provider is None: raise Blocked('long-run mock provider absent')
            provider.script.append({'name':'shell','arguments':{'command':'/usr/bin/python3 '+str(path),
                'timeout':LONG_RUN_SHELL_TIMEOUT_MS}})
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
                self._await_gone([identity],'idle retirement not reached in bound',
                                 seconds=IDLE_RETIREMENT_SECONDS)
                if not self.proc.gone(identity): raise Blocked('idle retirement not reached in bound')
                # §13: Host stops the vendor before final daemon shutdown releases
                # its locks. Wait for the natural idle end, without any CLI probe,
                # so the next bootstrap cannot auto-start behind a saved live PID.
                if self.daemon is None: raise Blocked('idle daemon identity absent')
                self._await_gone([self.daemon],'daemon idle exit not reached in bound',
                                 seconds=IDLE_RETIREMENT_SECONDS)
                self._http=None; self._event_generation=None; self._idle_retired=True
            return {'stop_proven':True,'vendor_session_id':sid}
        if kind=='via_events':
            events=read_pages(lambda after:self.via(['events',session,'--after',str(after)]))
            terminal=terminal_event(events,session,args.get('turn'))
            return {'complete':True,'events':events,'terminal_revision':terminal['revision']}
        if kind=='turn_identity':
            envelope=args['envelope']; sid=envelope['vendor_session_id']
            inputid=input_id(session,envelope['turn'])
            def ready(rows):
                delivered=[row for row in rows if row['type']=='session.inbox.delivered'
                           and row.get('data',row).get('inboxID',row.get('data',row).get('id'))==inputid]
                if len(delivered)>1: raise Blocked('exact current owned input delivery ambiguous')
                if not delivered: return False
                later=[row['seq'] for row in rows if row['type']=='session.inbox.delivered'
                       and row['seq']>delivered[0]['seq']]
                boundary=min(later,default=float('inf'))
                return any(delivered[0]['seq']<row['seq']<boundary and row['type'] in {
                    'session.execution.succeeded','session.execution.failed','session.execution.interrupted'} for row in rows)
            native=self._await_native(sid,ready,'owned native delivery/terminal unobservable')
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
            model=self._request('GET','/api/session/'+sid)
            ref=self._json(model[1])['data']['model']; identity=ref['providerID']+'/'+ref['id']
            if identity not in {safety.FREE_IDENTITY,safety.MOCK_IDENTITY}:
                self.guard.stopped=True; raise Blocked('paid model readback hard stop')
            return {'mode':'mock' if ref['providerID'] in self.provider_endpoints else 'public-free',
                    'model':identity,'owned_terminal':bool(owned),'execution_id':execution,'input_id':inputid,
                    'vendor_error_observed':any(row['type']=='session.step.failed' for row in native),
                    'automatic_compaction':any(row['type'].startswith('session.compaction.') for row in native)}
        if kind=='native_terminal':
            sid=self._vendor_sid(session)
            request=self.last_request
            if not request or request['receipt'].get('session_id')!=session:
                raise Blocked('native terminal current turn identity unavailable')
            try: number=int(request['receipt']['turn'].rsplit('/',1)[1])
            except (ValueError,KeyError,IndexError) as error:
                raise Blocked('native terminal current turn identity malformed') from error
            expected=input_id(session,number)
            def ready(rows):
                delivery=[event for event in rows if event['type']=='session.inbox.delivered'
                          and event.get('data',event).get('inboxID',event.get('data',event).get('id'))==expected]
                if len(delivery)>1: raise Blocked('native terminal delivery ambiguous')
                return bool(delivery) and any(event['seq']>delivery[0]['seq'] and event['type'] in {
                    'session.execution.succeeded','session.execution.failed','session.execution.interrupted'} for event in rows)
            rows=self._await_native(sid,ready,'native terminal absent')
            delivery=next(event for event in rows if event['type']=='session.inbox.delivered'
                          and event.get('data',event).get('inboxID',event.get('data',event).get('id'))==expected)
            terminal=[event for event in rows if event['seq']>delivery['seq'] and event['type'] in {
                'session.execution.succeeded','session.execution.failed','session.execution.interrupted'}]
            row=terminal[-1]; data=row.get('data',row)
            return {'type':row['type'],'reason':data.get('reason'),'owned':any(event['type']=='session.inbox.delivered' and event['seq']<row['seq']
                                and str(event.get('data',event).get('inboxID','')).startswith('msg_via') for event in rows)}
        if kind=='mock_receipt':
            row=next((provider for name,provider in self.mock_providers.items()
                      if self.fixtures.get(name,{}).get('path')==self.project),None)
            if row is None: raise Blocked('owned mock provider absent')
            receipt=row.receipt()
            if not receipt['received'] or not receipt['model_matches']:
                self.guard.stopped=True; raise Blocked('overridden provider failed mock receipt')
            return receipt
        if kind=='helper_barrier':
            if args.get('cancel_diagnostic') is True:
                self._cancel_diagnostic_session=self._vendor_sid(args['session'])
            request=self.last_request or {};receipt=request.get('receipt',{})
            session=args.get('session',receipt.get('session_id'))
            public=request.get('model')==safety.FREE_IDENTITY and session==receipt.get('session_id')
            seconds=PUBLIC_HELPER_READINESS_SECONDS if public else OWNED_READINESS_SECONDS
            if public:self._timeline_ready(session,seconds,observed=False)
            helper=args['helper']; path=self._helper_folder(self.project)/(helper+'.json')
            readiness_seen=False;not_ready_seen=False;failure_seen=None
            def read():
                nonlocal readiness_seen,not_ready_seen,failure_seen
                try: raw=path.read_bytes()
                except FileNotFoundError: return None
                readiness_seen=True
                if len(raw)>OBSERVATION_BYTES: raise Blocked('helper observation bound')
                row=self._json(raw)
                failure=self._helper_failure(row)
                if failure is not None:
                    if row.get('kind')!=helper:raise Blocked('helper failure kind mismatch')
                    not_ready_seen=True;failure_seen=failure
                    return None
                _typed(row,{'pid':I,'start_ticks':I,'ready':B,'kind':S},'helper readiness')
                identity=safety.Identity(row['pid'],row['start_ticks'])
                self._verify(identity)
                if row['kind']!=helper or not self._descends_from(identity,self.vendor_identity):
                    raise Blocked('helper readiness did not come from owned generation')
                if row['ready'] is not True:
                    not_ready_seen=True
                    return None
                self.identities.add(identity); self.helper_ledger[identity]=dict(row)
                self.helper_generations[identity]=self.vendor_identity; self.helper_origins[identity]=path.parent
                return {'started':True,'owned':True,**row}
            try:
                result=self._await_owned(read,'helper barrier not reached',seconds=seconds)
                if public:self._timeline_ready(session,seconds)
                return result
            except Blocked as error:
                if str(error)=='helper barrier not reached':
                    try:
                        self._record('helper-readiness',{'timed_out':True,'bound_seconds':seconds,
                            'folder_exists':path.parent.exists(),'readiness_file_exists':path.exists(),
                            'readiness_seen':readiness_seen,'not_ready_seen':not_ready_seen,
                            'failure':failure_seen})
                    except BaseException:
                        error.helper_readiness_retention_failed=True
                raise
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
            sid=self._vendor_sid(session)
            native=self.observe('native_events',sid)['events']
            if 'wire.http.before_response' in held:
                request=self.last_request
                if not request or request['receipt'].get('session_id')!=session:
                    raise Blocked('cancel timing current turn identity unavailable')
                try: number=int(request['receipt']['turn'].rsplit('/',1)[1])
                except (KeyError,ValueError,IndexError) as error:
                    raise Blocked('cancel timing current turn identity malformed') from error
                expected=input_id(session,number)
                def interrupted(rows):
                    delivery=[row['seq'] for row in rows if row['type']=='session.inbox.delivered'
                        and row.get('data',row).get('inboxID',row.get('data',row).get('id'))==expected]
                    if len(delivery)>1: raise Blocked('cancel timing delivery ambiguous')
                    return bool(delivery) and any(row['seq']>delivery[0]
                        and row['type']=='session.execution.interrupted'
                        and row.get('data',row).get('reason')=='user' for row in rows)
                native=self._await_native(sid,interrupted,'held response native interruption unobservable')
            interrupts=[row for row in native if row['type']=='session.execution.interrupted' and row.get('data',row).get('reason')=='user']
            proof=self._fake_control('qualification_body_prefix_cancel_preserves_offset_and_stop_pool')
            return {'cancel_returned':type(self.last_cancel['reply'].get('state')) is str,
                    'cancel_ms':self.last_cancel['elapsed_ms'],'prompt_held':bool(held),
                    'native_user_interrupted':bool(interrupts),
                    'sourcebound_pool_test':proof}
        if kind=='write_observation':
            retiring=self._host_record()
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
            # §8/§13 L2: keep the original HTTP pause through its own deadline.
            # Core unknown alone does not establish an inconclusive HTTP result.
            original=[row for row in acks if row.get('occurrence')==1]
            release=Path(self.env['VIA_FAILPOINT_DIR'])/(args['point']+'.1.release')
            if len(original)!=1 or release.exists():
                raise Blocked('original HTTP deadline barrier not held')
            deadline=min(time.monotonic()+WRITE_DRAIN_SECONDS,self.phase_deadline or float('inf'))
            self._await_gone([self.vendor_identity,self.anchor],'indeterminate generation drain unverified',
                             seconds=max(0,deadline-time.monotonic()),guard=lambda:self._verify(self.daemon))
            while True:
                if self.signals:self.signals.guard()
                self._verify(self.daemon)
                current=self._host_record(allow_absent=True)
                if current is None:break
                if current!=retiring:raise Blocked('draining Host generation changed')
                if time.monotonic()>=deadline:raise Blocked('draining Host absence commit unverified')
                time.sleep(min(.02,max(0,deadline-time.monotonic())))
            self.close_event_capture()
            self._http=None
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
            accepted=any(row['type']=='turn.started' for row in current)
            terminal=terminal_event(events,session,number,required=False)
            known=terminal is not None and terminal['state']!='unknown'
            return {'accepted_before_loss':accepted,'via_terminal_admitted':known}
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
                if alive is None: raise safety.process_block('helper',None,'predecessor helper identity unverifiable')
                if alive is False: continue
                self._verify(identity)
                folder=self.helper_origins.get(identity)
                name=self.helper_ledger[identity].get('kind')
                if folder is None or type(name) is not str or not name.replace('-','').replace('_','').isalnum():
                    raise Blocked('predecessor helper release channel unavailable')
                for channel in (name,'marker-parent') if name=='marker' else (name,):
                    path=folder/(channel+'.release')
                    if not path.exists():
                        fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.close(fd)
            self._await_gone(helpers,'predecessor helper survives cooperative release',
                             seconds=HELPER_STOP_SECONDS)
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
            deadline=min(args['deadline'],self.phase_deadline or args['deadline'])-L9_FINISH_SECONDS
            if deadline<=beginning:raise Blocked('L9 sampling window exhausted before completion reserve')
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
                    'finish_reserve_seconds':L9_FINISH_SECONDS,
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
        self._verify(identity); self._verify(ancestor)
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
        self._cleanup_active=True
        try: return self._finish()
        finally: self._cleanup_active=False

    def _finish(self):
        """§13: cleanup proofs use their own bounds even after a phase expires."""
        failures=[];traffic={'received_requests':0,'providers':0}
        diagnostic_error=None
        if self.diagnostic_mock_only:
            try:
                from opencode_diagnostic import capture
                self._record('mock-diagnostic-before-cleanup',capture(self.work,self.ownership,self._diagnostic_sessions,
                    replacements=(*self.vault.redaction_forms(),*self.secret_forms,*self.bearer_forms)))
            except BaseException as error:diagnostic_error=error
        try:self._timeline_flush('model-timelines-before-cleanup')
        except BaseException as error:diagnostic_error=error
        if self.external_root:
            try:
                from opencode_runroot import vendor_message_facts
                self._record('native-messages-before-cleanup',{'sources':vendor_message_facts(
                    self.work,self._reply_protected,self.ownership,
                    cancel_session=self._cancel_diagnostic_session)})
            except BaseException as error:
                if diagnostic_error is None:diagnostic_error=error
        try: proof=self.stop()
        except BaseException as error: failures.append(error)
        finally:
            self._publication_release.set()
            for provider in self.mock_providers.values():
                try: provider.__exit__(None,None,None)
                except BaseException as error:
                    failure=Blocked('owned mock provider cleanup unverified')
                    failure.__cause__=error
                    failures.append(failure)
            try:
                receipts=[{'fixture':name,**{key:provider.receipt()[key] for key in (
                    'requests','admitted_requests','refused_requests','unreleased_responses',
                    'connection_limit_reached','connection_limit')}}
                    for name,provider in sorted(self.mock_providers.items())]
                traffic={'received_requests':sum(row['requests'] for row in receipts),
                         'admitted_requests':sum(row['admitted_requests'] for row in receipts),
                         'refused_requests':sum(row['refused_requests'] for row in receipts),
                         'unreleased_responses':sum(row['unreleased_responses'] for row in receipts),
                         'providers':len(receipts),'receipts':receipts}
                self._record('mock-traffic',traffic)
                usage_provider=self.mock_providers.get('usage-cache')
                if usage_provider is not None:
                    self._record('mock-exchanges',{'fixture':'usage-cache',
                        'exchanges':usage_provider.exchange_records()})
                if any(row['connection_limit_reached'] for row in receipts):
                    failures.append(Blocked('mock provider connection ceiling exhausted'))
            except BaseException as error:
                failure=Blocked('mock received request aggregate unverified')
                failure.__cause__=error;failures.append(failure)
            self.mock_providers.clear()
        try:self._timeline_flush('model-timelines')
        except BaseException as error:
            if diagnostic_error is None:diagnostic_error=error
        if diagnostic_error is not None:
            diagnostic_error.diagnostic_failure=True
            failures.append(diagnostic_error)
        if failures: raise failures[0]
        if self.diagnostic_mock_only:
            from opencode_diagnostic import capture
            self._record('mock-diagnostic',capture(self.work,self.ownership,self._diagnostic_sessions,
                replacements=(*self.vault.redaction_forms(),*self.secret_forms,*self.bearer_forms)))
        safety.verify_binary(self.pinned)
        if self.external_root:
            self._check_ancestors('finish')
            for kind,path in self.paths.items():
                expected=self.build_hashes.get(kind)
                if expected is not None and safety.sha256(path)!=expected:
                    raise Blocked('private VIA build changed during qualification')
            from opencode_runroot import native_facts
            self._record('native-records',{'sources':native_facts(self.work,self._reply_protected,self.ownership,
                cancel_session=self._cancel_diagnostic_session)})
        scan=self.secrecy_scan()
        if scan['secret_absent'] is not True or scan['payload_captures']!=0:
            raise Blocked('final secrecy scan failed')
        return {'stopped':True,**proof,'pinned_rehash':True,'secrecy_scan':scan,'mock_traffic':traffic}

    def _check_ancestors(self,stage):
        """Repeat §13 external-root checks before admission and final native/secret reads."""
        if not self.external_root: return
        from opencode_runroot import check_ancestors
        try: report=check_ancestors(self.work)
        except Blocked as error:
            self._record('ancestor-check',{'stage':stage,**getattr(error,'ancestor_discovery',{})})
            raise
        self._record('ancestor-check',{'stage':stage,**report})

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
        data=error.get('data')
        if error.get('code')==-32012 and type(data) is dict \
                and data.get('kind')=='admission_refused' and data.get('kind2')=='disk_free_floor':
            _typed(data,{'free_bytes':I,'floor_bytes':I},'disk free floor refusal')
            raise Blocked('private VIA state is below its configured disk free floor')
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
        """§13: retain each sentinel's received requests even when its turn blocks."""
        self.start(self.phase_kind or 'release',project)
        self.ensure_vendor()  # Bootstrap receipts are counted separately, before this baseline.
        names=[name for name,row in self.fixtures.items() if row['path']==project]
        if len(names)!=1 or names[0] not in self.mock_providers:
            raise Blocked('mock fixture receipt identity unavailable')
        name=names[0];provider=self.mock_providers[name];before=provider.requests
        bootstrap_before=self._bootstrap_count
        failure=None
        try:
            receipt=self.via(['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                              '--cwd',str(project),'--bound','full','--network','--background','--prompt',prompt])
            if 'cli_error' in receipt: raise Blocked('local fixture turn refused')
            result=self.via(['wait',receipt['turn'],'--timeout-ms','180000'])
            if result['state']!='completed': raise Blocked('local fixture turn did not complete')
            return receipt,result
        except BaseException as error:
            failure=error;raise
        finally:
            try:
                self._record('mock-receipt',{'fixture':name,'received_requests':provider.requests-before,
                    'label':'sentinel','charged':'mock-only','gate':False,
                    'count_scope':'includes-rebootstrap-traffic',
                    'rebootstraps_during_turn':self._bootstrap_count-bootstrap_before})
            except BaseException:
                if failure is None: raise
                failure.mock_receipt_failed=True

    def _instruction_facts(self):
        """Read owned native instruction-state hashes/marker Booleans only (§13)."""
        from opencode_runroot import instruction_facts
        rows=[]
        for database in sorted(self.namespace.rglob('opencode.db')):
            rows.extend(instruction_facts(database))
        if not rows: raise Blocked('native instruction-state evidence unavailable')
        return rows

    def _repository_sentinel(self):
        """First model-capable phase uses only loopback controls with nested Git sentinels."""
        outer=self._directory(self.work/'ancestor-fixture','vendor-private')
        safety.init_fixture_repo(outer,self.home,sentinel=True)
        nested=self.ownership.register(outer/'project-boundary','vendor-private')
        safety.init_fixture_repo(nested,self.home)
        control=self._directory(outer/'walk-up-control','vendor-private')
        boundary_results=[]
        projects=dict(zip(REPOSITORY_SENTINELS,(nested,control,self.namespace)))
        for name in REPOSITORY_SENTINELS:
            project=projects[name]
            self._directory(project/'.opencode','vendor-private')
            config=self._materialize_fixture(name,project,{'provider':'mock'})
            self._write_fixture_config(project,config)
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
            raise Blocked('ancestor sentinel control not sensitive')
        for result in (project,namespace):
            if not result['local_marker'] or result['ancestor_agents']:
                raise Blocked('vendor repository walk-up crossed private Git boundary')
        native=self._instruction_facts()
        self._record('ancestor-skills',{'disposition':'record-only','gate':False,
            'crossed':project['ancestor_skills'],'namespace_crossed':namespace['ancestor_skills'],
            'native_instruction_state':native,'marker_receipts':[{key:row[key] for key in
                ('local_marker','ancestor_agents','ancestor_skills')} for row in boundary_results]})
        models=[]
        for result in boundary_results:
            observed=result.get('models')
            if type(observed) is not list or not observed or any(value!='fixture-free' for value in observed):
                raise Blocked('auxiliary mock model receipts unobservable')
            models.extend(safety.MOCK_IDENTITY for value in observed)
        self.admit_automatic_models(models,mock_received=all(row['received'] for row in boundary_results),
                                    configuration_immutable=bool(self._last_auxiliary_bindings))
        return {'project_boundary':True,'namespace_boundary':True,'ancestor_control_sensitive':True,
                'ancestor_agents_absent':True,'private_git_commit':True}

    def _foreign_observation(self,session,args,held=False):
        if not hasattr(self,'_foreign'): raise Blocked('foreign input not injected')
        sid=self._vendor_sid(session)
        def ready(events):
            delivery=[row['seq'] for row in events if row['type']=='session.inbox.delivered'
                      and row.get('data',row).get('inboxID',row.get('data',row).get('id'))==self._foreign['id']]
            if len(delivery)>1: raise Blocked('foreign delivery identity ambiguous')
            if not delivery: return False
            return held or any(row['seq']>delivery[0] and row['type'] in {
                'session.execution.succeeded','session.execution.failed','session.execution.interrupted'} for row in events)
        events=self._await_native(sid,ready,'foreign execution delivery/terminal unobservable')
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
        terminal=terminal_event(c1,session,number,required=False)
        accepted=any(row['type']=='turn.started' for row in current)
        result={'owned_not_submitted':absent,
                'foreign_not_credited':terminal is None or terminal['state']!='completed',
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
        removed=self.vendor('DELETE','/api/session/'+sid)
        if removed['status'] not in {200,204}: raise Blocked('owned vendor session delete refused')
        self._idle_retired=False
        self._operation('idle_retirement',{'session':session})
        self.ensure_vendor()
        # Establish the baseline after successor bootstrap; that bootstrap's
        # native session is owned auxiliary work, never a replacement for sid.
        before=self.vendor('GET','/api/session')
        missing=self.vendor('GET','/api/session/'+sid)
        if missing['status']!=404: raise Blocked('missing-ID refusal control lacks verified missing session')
        if self.last_model!=safety.MOCK_IDENTITY:
            raise Blocked('missing-ID refusal control requires mock identity')
        self.spending_check(args=['spawn','--model',safety.MOCK_IDENTITY,'--cwd',str(self.project)])
        command=['resume',session,'--prompt','VIA missing-ID control']
        self._metadata_command=command;self._metadata_active=True
        try:
            reply=self.via(command)
            if 'cli_error' in reply: raise Blocked('missing-ID resume not receipted')
            envelope=self.via(['wait',reply['turn'],'--timeout-ms','180000'])
            if self.guard.stopped: raise Blocked('missing-ID control attempted model work')
        finally:
            self._metadata_command=None;self._metadata_active=False
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
            entries=reply.get('body')
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
            def read_completed(events):
                # §13: the native called event omits name; input.started owns it.
                def key(row):
                    data=row.get('data',{})
                    return (data.get('sessionID'),data.get('assistantMessageID'),data.get('id'))
                started={key(row) for row in events
                         if row.get('type')=='session.tool.input.started'
                         and row.get('data',{}).get('name')=='read'}
                called={key(row) for row in events if row.get('type')=='session.tool.called'}
                succeeded={key(row) for row in events if row.get('type')=='session.tool.success'}
                completed={row[2] for row in started & called & succeeded
                           if row[0]==sid and all(type(value) is str for value in row)}
                return any(row.get('id') in completed for row in calls)
            self._await_native(sid,read_completed,'configured fixture file read was not actually completed')
            beginning=time.monotonic(); deadline=beginning+5
            if self.phase_deadline is not None and deadline>self.phase_deadline:
                raise Blocked('phase has insufficient LSP readiness budget')
            spawned=False; marker=self._helper_folder(project)/'lsp.json'
            verify=self._owned_observation_guard()
            while True:
                if self.signals: self.signals.guard()
                verify()
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
        # C1 §3.13 models only lists. A spawn runs §4.3's credential
        # handshake; the deliberately unoffered effort is a second, pre-native
        # refusal fence should the known-credential check unexpectedly admit.
        command=['spawn','--harness','opencode','--model',safety.MOCK_IDENTITY,
                 '--cwd',str(self.namespace),'--bound','full','--network',
                 '--effort',METADATA_EFFORT,'--background','--prompt','VIA FENCE ONLY','--json']
        def attempt():
            self._check_ancestors('metadata')
            self._metadata_command=command; self._metadata_active=True
            try:
                receipt=self.via(command)
                if 'cli_error' in receipt: raise Blocked('credential acquisition refused before dispatch')
                result=self.via(['wait',receipt['turn'],'--timeout-ms','30000'])
                failure=result.get('failure') or {}
                # The fixed step is authored by VIA, not the vendor. Keep only
                # its Boolean match; never write the returned message.
                if result['state']!='failed' or failure.get('class')!='submit_failed' \
                        or (failure.get('data') or {}).get('reason')!='launch_failed' \
                        or 'check credential state' not in failure.get('message','') \
                        or result['vendor_session_id'] is not None:
                    raise Blocked('known credentials not refused')
            finally:
                self._metadata_command=None; self._metadata_active=False
        attempt(); after=self._anchor_rows(); attempt(); twice=self._anchor_rows()
        if len(twice)<=len(after) or len(after)<=len(before):
            raise Blocked('credential refusal caching proof unavailable')
        return {'code':'unexpected_credential_state','cached':False,'via_sessions_created':2,
                'session_creates':0,'prompt_submits':0,'credential_gets':0}

    def _settle_near_limit_claim(self,pending):
        """C1 P6: settle the held unsent claim before restart; unknown fences successors."""
        session,number=pending['turn'].rsplit('/',1)
        self.via(['cancel',session,'--turn',number])
        terminal=self.via(['wait',pending['turn'],'--timeout-ms','180000'])
        if terminal.get('state')!='cancelled' or (terminal.get('cancel') or {}).get('cleanup')!='quiescent':
            raise Blocked('near-limit predecessor not safely settled')

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
        # The claim has not crossed the eligibility barrier, so cancelling it
        # withdraws only VIA work. A crash would recover unknown and C1 P6
        # would cancel the successor before the adapter can reopen anything.
        self._settle_near_limit_claim(pending)
        self.stop(); self.start('failpoints',project); self.ensure_vendor()
        durable=self.vendor('GET','/api/session/'+sid+'/inbox')
        items=durable['body'].get('data')
        if type(items) is not list or {row.get('id') for row in items}!={expected,foreign}:
            raise Blocked('parked near-limit queue durability unobserved')
        from opencode_reply import reply_projection
        status=self.via(['status',session])
        self._record('near-limit-via-state',{'stage':'before-successor',
            **reply_projection(json.dumps(status).encode(),self.reply_evidence.protected)})
        successor=self.via(['resume',session,'--prompt','VIA SUCCESSOR'])
        envelope=self.via(['wait',successor['turn'],'--timeout-ms','180000'])
        if envelope['state']!='completed':
            events=read_pages(lambda after:self.via(['events',session,'--after',str(after)]))
            self._record('near-limit-via-state',{'stage':'successor-incomplete',
                **reply_projection(json.dumps({'data':{'snapshot':envelope,'events':events}}).encode(),
                                   self.reply_evidence.protected)})
            raise Blocked('near-limit successor did not complete')
        events=self._await_native(sid,lambda rows:any(row['type']=='session.inbox.cancelled'
            and row.get('data',row).get('inboxID',row.get('data',row).get('id'))==expected for row in rows),
            'owned near-limit cancellation event unobservable')
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
        events=self._await_native(sid,lambda rows:any(row['type']=='session.inbox.enqueued'
                                  for row in rows),'VIA owned accepted input not observed')
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

    def _mock_request_admit(self,model,*,fixture=None):
        """Count every local provider request, including title/child/compaction calls."""
        if self._metadata_active:
            self.guard.stopped=True; raise Blocked('model request during metadata acquisition')
        if model!='fixture-free':
            self.guard.stopped=True; raise Blocked('paid or unexpected auxiliary mock identity')
        if self.signals: self.signals.guard()
        if self.phase_deadline and time.monotonic()>=self.phase_deadline:
            self.guard.stopped=True; raise Blocked('mock request after phase deadline')
        if self.guard.stopped or self.phase_mock_left is None or self.phase_mock_left<=0:
            self.guard.stopped=True; raise Blocked('mock model request ceiling exhausted')
        if fixture in self._mock_provider_left:
            if self._mock_provider_left[fixture]<=0:
                self.guard.stopped=True; raise Blocked('mock provider request ceiling exhausted')
            self._mock_provider_left[fixture]-=1
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
