"""Bounded, allow-listed reply evidence for OpenCode qualification (§13).

Raw replies never reach a file. Projections are diagnostic context, never
spending or qualification proof, and preserve uncertainty when truncated.
"""
import contextlib
import functools
import hashlib
import json
import math
import re
import threading
import urllib.parse
from collections import OrderedDict

import opencode_safety as safety
from opencode_c1 import (EVENT_TYPES, ERROR_CODES, CANCEL_OUTCOMES, CLEANUP_STATES,
                        FAILURE_CLASSES, WARNING_CODES, STOP_REASONS, DENIAL_KINDS)

# §13: SessionV1 and SDK error literals from hash-verified 2.0.22; never error text.
VENDOR_ERROR_NAMES = frozenset({'provider.auth','provider.rate-limit','provider.quota',
    'provider.content-filter','provider.transport','provider.internal','provider.invalid-output',
    'provider.invalid-request','provider.unsupported-operation','provider.no-route',
    'provider.timeout','provider.unknown','provider.error','aborted','unknown',
    'permission.rejected','tool.execution','tool.interrupted',
    # Hash-verified 2.0.22 SessionV1 error union at executable offset 144406910.
    'MessageOutputLengthError','ProviderAuthError','MessageAbortedError',
    'StructuredOutputError','APIError','ContextOverflowError','ContentFilterError',
    'AI_APICallError','AI_DownloadError','AI_EmptyResponseBodyError',
    'AI_InvalidArgumentError','AI_InvalidPromptError','AI_InvalidResponseDataError',
    'AI_JSONParseError','AI_LoadAPIKeyError','AI_LoadSettingError',
    'AI_NoContentGeneratedError','AI_NoSuchModelError','AI_TooManyEmbeddingValuesForCallError',
    'AI_TypeValidationError','AI_UnsupportedFunctionalityError'})

REPLY_BYTES = 16 * 1024 * 1024  # §9/§13: the runner's existing observation cap.
PROJECTION_BYTES = 64 * 1024  # §13: diagnostic fields, not a second payload sink.
PROJECTION_NODES = 2048  # §13: bounded diagnostic traversal, including discarded fields.
PROJECTION_DEPTH = 12  # §13: diagnostic traversal cannot recurse with vendor depth.
PROJECTION_ITEMS = 64  # §13: label truncation rather than retain unbounded collections.
REPLY_CONTEXTS = 32  # §13: bounded recent replies for checks after a driver return.
ID_CHARS = 256  # §13: same identifier policy as catalogue diagnostic evidence.

IDS = frozenset({'id','providerID','modelID','sessionID','session_id','vendor_session_id',
                 'inboxID','inputID','messageID','parentID','agentID','model','agent','harness'})
NUMBERS = frozenset({'pid','code','seq','turn','n','revision','current_step','queue_position',
    'event_seq','next_after','steps','max_steps','usd',
    'input','output','cache_read','cache_write','read','write','context','total',
    'input_tokens','output_tokens','cached_input_tokens','reasoning_output_tokens',
    'total_tokens','tokens_input','tokens_output','tokens_cache_read','tokens_cache_write',
    'duration_ms','denied_actions_total','auto_declined_requests_total','size','count',
    'free_bytes','floor_bytes','free_floor','statusCode'})
BOOLS = frozenset({'more','stopping','already_terminal','vendor_identity_verified',
                  'incomplete','complete','received','model_matches','disabled','accepted','enabled','below_free_floor','isRetryable'})
ENUM_KEYS = frozenset({'type','state','status','scope','support','kind','kind2','class','outcome',
                       'cleanup','admission','phase','effect','action','version_status','field',
                       'provenance','stop_reason'})
ENUMS = frozenset({'document','directory','running','active','idle','queued','submitting','accepted',
    'completed','failed','cancelled','unknown','closed','open','fenced','closing',
    'server','turn','vendor_interval','session_cumulative','unavailable','reported','derived',
    'supported','unsupported','native','best_effort','tested','untested','allow','deny','ask',
    'shell','read','write','edit','task','subagent','question','text','tool','reasoning',
    'deprecated','alpha','beta','stable','preview','done','none','pending','interrupted',
    'submission_unknown','launch_failed','handshake_refused','invalid_params','submit_failed',
    'admission_refused','disk_free_floor',
    'provider.error','rate_limit','quota','context','provider.updated','config.updated',
    'session.created','session.updated','session.deleted','session.idle',
    'session.message.updated','session.message.created','session.execution.started',
    'session.execution.succeeded','session.execution.failed','session.execution.interrupted',
    'session.inbox.enqueued','session.inbox.delivered','session.inbox.cancelled',
    'max_steps','model','tools','estimated'})|EVENT_TYPES|frozenset(ERROR_CODES) \
    |CANCEL_OUTCOMES|CLEANUP_STATES|FAILURE_CLASSES|WARNING_CODES|STOP_REASONS|DENIAL_KINDS
CONTAINERS = frozenset({'data','info','model','cost','usage','cache','settings','effective',
    'leftovers','processes','failure','error','cancel','progress','capabilities','params','max_steps','events',
    'content','snapshot','turns','active_turn','inherit','warnings','denied_actions','limits','disk','storage'})
MAPS = frozenset({'providers','agents','models','variants'})
PRIVATE = frozenset({'handle','token','password','apikey','api_key','authorization',
                     'headers','credential','credentials','secret','access_token','refresh_token'})


def origin_class(value):
    """§13: retain only a literal loopback host:port, never URL credentials/path."""
    if type(value) is not str:
        return {'class':'non-loopback-or-unverifiable'}
    try:
        parsed=urllib.parse.urlsplit(value)
        if parsed.username is not None or parsed.password is not None:
            return {'class':'credentialed'}
        host=parsed.hostname;port=parsed.port
        if parsed.scheme not in {'http','https'} or host not in {'127.0.0.1','::1','localhost'} \
                or port is None:
            return {'class':'non-loopback-or-unverifiable'}
        return {'class':'loopback','host_port':('['+host+']' if host=='::1' else host)+':'+str(port)}
    except (ValueError,TypeError):
        return {'class':'non-loopback-or-unverifiable'}


def reply_projection(raw,protected):
    """§13: project fixed fields; unknown strings and secret-bearing fields disappear."""
    record={'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest(),
            'decode_status':'unavailable','projection':None,'truncated':False}
    if len(raw)>REPLY_BYTES:
        record['truncated']=True
        return record
    try:
        value=json.loads(raw,parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
    except (ValueError,UnicodeError,TypeError,RecursionError):
        # Foreground C1 spawn publishes a receipt and envelope on separate
        # lines. Collect private fields across both before projecting either.
        lines=raw.splitlines()
        if len(lines)>PROJECTION_ITEMS:
            record['truncated']=True
            return record
        try:
            if len(lines)<2: raise ValueError()
            value=[json.loads(line,parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
                   for line in lines]
        except (ValueError,UnicodeError,TypeError,RecursionError):
            record['decode_status']='invalid-json'
            return record
        record['decode_status']='json-lines'
    else: record['decode_status']='decoded'
    private=[];visited=0;incomplete=False
    def secrets(value,depth=0,hidden=False):
        nonlocal visited,incomplete
        visited+=1
        if visited>PROJECTION_NODES or depth>PROJECTION_DEPTH:
            incomplete=True;return
        if type(value) is dict:
            for key,item in value.items():
                secrets(item,depth+1,hidden or key.lower() in PRIVATE)
                if visited>PROJECTION_NODES: break
        elif type(value) is list:
            if len(value)>PROJECTION_ITEMS: incomplete=True
            for item in value[:PROJECTION_ITEMS]: secrets(item,depth+1,hidden)
        elif hidden and type(value) is str and value:
            private.append(value.encode('utf-8','surrogatepass'))
    secrets(value)
    if incomplete:
        record['truncated']=True
        return record
    def hidden(raw):
        return protected(raw) or any(form in raw for form in private)
    def identifier(value):
        return value if type(value) is str and len(value)<=ID_CHARS \
            and re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]*(?:/[A-Za-z0-9][A-Za-z0-9._-]*)?',value) \
            and not hidden(value.encode()) else None
    def number(value):
        try:
            return value if type(value) in {int,float} and math.isfinite(value) \
                and not hidden(str(value).encode()) else None
        except OverflowError: return None
    nodes=0
    def walk(value,key=None,depth=0):
        nonlocal nodes
        nodes+=1
        if nodes>PROJECTION_NODES or depth>PROJECTION_DEPTH:
            record['truncated']=True
            return None
        if type(value) is list:
            if len(value)>PROJECTION_ITEMS: record['truncated']=True
            return [walk(item,None if key in MAPS else key,depth+1) for item in value[:PROJECTION_ITEMS]]
        if type(value) is dict:
            result={}
            for field,item in value.items():
                if nodes>PROJECTION_NODES:
                    record['truncated']=True;break
                nodes+=1
                if field.lower() in PRIVATE: continue
                if key in MAPS:
                    name=identifier(field)
                    if name is not None: result[name]=walk(item,None,depth+1)
                elif field in CONTAINERS|MAPS or field in IDS and type(item) is dict:
                    result[field]=walk(item,field,depth+1)
                elif field in IDS:
                    result[field]=number(item) if type(item) is int else identifier(item)
                elif field=='code' and type(item) is str:
                    result[field]=item if item in WARNING_CODES|{'launch_failed'} \
                        and not hidden(item.encode()) else 'unrecognized'
                elif field in NUMBERS: result[field]=number(item)
                elif field in BOOLS: result[field]=item if type(item) is bool else None
                elif field=='name' and key=='error':
                    result[field]=item if type(item) is str and item in VENDOR_ERROR_NAMES \
                        and not hidden(item.encode()) else 'other'
                elif field in ENUM_KEYS:
                    result[field]=item if type(item) is str and item in ENUMS|VENDOR_ERROR_NAMES \
                        and not hidden(item.encode()) else 'unrecognized'
                elif field in {'version','vendor_version'}:
                    result[field]=item if type(item) is str and re.fullmatch(r'\d+\.\d+\.\d+',item) \
                        and not hidden(item.encode()) else None
                elif field=='baseURL':
                    origin=origin_class(item)
                    result[field]={'class':'redacted'} if hidden(json.dumps(origin).encode()) else origin
            return result
        if key in IDS: return identifier(value)
        return number(value) if key in NUMBERS else None
    record['projection']=walk(value)
    if len(json.dumps(record).encode())>PROJECTION_BYTES:
        record['projection']=None;record['truncated']=True
    return record


class ReplyEvidence:
    """§13: retain scoped/recent projections at a block, with no raw reply cache."""
    def __init__(self,emit,protected,context,*,vault=None,secret_forms=lambda:()):
        self.emit,self.protected,self.context=emit,protected,context
        self.vault,self.secret_forms=vault,secret_forms
        self._recent=OrderedDict();self._local=threading.local();self._lock=threading.RLock()

    def capture(self,source,route,raw,**status):
        """§13: transport receipt precedes decoding, schema and verdict checks."""
        metadata={key:value for key,value in status.items()
                  if key in {'http_status','exit_code'} and type(value) is int
                  or key in {'complete','body_read'} and type(value) is bool}
        # Transport finally callbacks must never replace their original outcome.
        try:
            record={'source':source,'route':route,**metadata,**self.context(),
                    **reply_projection(raw,self.protected),'identity_verified':False}
        except Exception:
            record={'source':source,'route':route,'projection_failed':True,
                    'identity_verified':False}
        key=(source,route,record.get('location'))
        with self._lock:
            self._recent.pop(key,None);self._recent[key]=record
            while len(self._recent)>REPLY_CONTEXTS: self._recent.popitem(last=False)
            for frame in getattr(self._local,'frames',[]):
                frame.append(record)
                if len(frame)>REPLY_CONTEXTS: del frame[0]

    def snapshot(self,sources):
        """§13: sanitized memory-only context for main-thread stream retention."""
        with self._lock:
            return [row for row in self._recent.values() if row['source'] in sources]

    def reset(self):
        """§13: a new phase cannot present earlier phase replies as current context."""
        with self._lock: self._recent.clear()

    def block(self,error,replies=None):
        """§13: attach the evidence file to the original block; failed retention stays blocked."""
        if getattr(error,'reply_evidence',None): return error.reply_evidence
        with self._lock:
            records=list(self._recent.values()) if replies is None else list(replies)
        if not records: return []
        try:
            name=self.emit('reply-block',{'blocking':safety.blocking_record(error,
                vault=self.vault,secret_forms=self.secret_forms()),
                'association':'recent-context' if replies is None else 'evaluation-scope',
                'replies':records})
            error.reply_evidence=[name] if type(name) is str else []
        except BaseException:
            error.reply_retention_failed=True
        return getattr(error,'reply_evidence',[])

    @contextlib.contextmanager
    def check(self):
        frames=getattr(self._local,'frames',None)
        if frames is None: frames=[];self._local.frames=frames
        frame=[];frames.append(frame)
        try: yield
        except safety.Blocked as error:
            # A validator can consume a reply returned by its caller. The
            # bounded recent context is explicitly labelled in that case.
            self.block(error,frame if frame else None)
            raise
        finally: frames.pop()


def reply_check(function):
    """§13: one evaluation guard shared by all reply-checking driver boundaries."""
    @functools.wraps(function)
    def checked(self,*args,**kwargs):
        with self.reply_evidence.check():
            return function(self,*args,**kwargs)
    return checked
