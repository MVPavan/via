"""Explicit mock-only error diagnostics, never public-session content (§13)."""
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import urllib.parse

import opencode_safety as safety

# §13: only cases whose model-capable operations are scripted loopback requests.
MOCK_CASES={'usage':'ledger','compaction':'ledger','config':'isolation',
            'hostile_provider':'hostile','never_ask':'helpers','marker':'helpers','auth':'helpers',
            'write_cancel':'seams','foreign':'seams','transport_loss':'seams','identity':'seams',
            'error_shapes':'seams','long_run':'long_run','anchor':'anchor',
            'credential_shape':'credentials'}
DIAGNOSTIC_BYTES=1024*1024  # §13: error/log text is bounded separately from payloads.
DIAGNOSTIC_ROWS=1024  # §13: only enrolled mock-session failures, no database dump.
DIAGNOSTIC_ENTRIES=100000  # §13: bounded discovery walk over the private run root.
DIAGNOSTIC_SOURCES=1024  # §13: databases plus vendor logs.
DIAGNOSTIC_PARTS=4096  # §13: parts examined per message for tool-state errors.
DIAGNOSTIC_INPUT_BYTES=256*1024*1024  # §13: aggregate enrolled message bytes read.
DIAGNOSTIC_LOG_BYTES=16*DIAGNOSTIC_BYTES  # §13: per-log snapshot bound.
DIAGNOSTIC_LOGS_BYTES=128*DIAGNOSTIC_BYTES  # §13: aggregate log bytes read.
BOOT_DIRECTORY=re.compile(r'(?:^|\s)directory=("(?:[^"\\]|\\.)*"|\S+)')


def exception_sites(error):
    """§13 mock diagnostics: runner source sites, never exception text or locals."""
    repository=Path(__file__).resolve().parents[2];rows=[];tb=error.__traceback__
    while tb is not None:
        code=tb.tb_frame.f_code;path=Path(code.co_filename)
        if path.is_absolute() and path.is_relative_to(repository/'scripts'/'qualify') \
                and re.fullmatch(r'opencode[a-z_]*\.py',path.name):
            rows.append({'file':path.relative_to(repository).as_posix(),
                         'function':code.co_name,'line':tb.tb_lineno})
        tb=tb.tb_next
    return rows[-32:]


def selection(names):
    """Preflight plus explicitly named mock cases; no public phase admission (§13)."""
    if not names or any(name not in MOCK_CASES for name in names):
        raise safety.Blocked('mock diagnostic case selection invalid')
    from opencode_cases import PHASES, PHASE_CASES
    chosen=set(names)
    return {'preflight':('preflight',),**{row.name:tuple(name for name in PHASE_CASES[row.name]
        if name in chosen) for row in PHASES if row.name in {MOCK_CASES[name] for name in chosen}}}


def redact(text,root,replacements=()):
    """Remove protected representations before the sink (§13).

    Exact run-root forms are replaced cosmetically; the run root is not
    protected material in mock-only local diagnostics (coordinator ruling)."""
    if type(text) is not str or len(text.encode())>DIAGNOSTIC_BYTES:
        raise safety.Blocked('mock diagnostic text bound')
    raw=text.encode()
    root_text=str(root)
    pairs=[(form,b'<private-run-root>') for form in {
        root_text.encode(),urllib.parse.quote(root_text,safe='').encode(),
        json.dumps(root_text)[1:-1].encode()}]
    for value in replacements:
        if not value:continue
        forms={value,base64.b64encode(value),base64.b64encode(b'opencode:'+value),
               value.hex().encode(),urllib.parse.quote_from_bytes(value).encode()}
        try:forms.add(json.dumps(value.decode())[1:-1].encode())
        except UnicodeError:pass
        pairs.extend((form,b'<redacted>') for form in forms)
    for value,replacement in sorted(pairs,key=lambda pair:(-len(pair[0]),pair[0])):
        raw=raw.replace(value,replacement)
    result=raw.decode('utf-8','replace')
    _check_protected(result,replacements)
    return result


def _check_protected(text,replacements):
    """§13: one shared representation-aware check for passwords, handles and
    synthetic secrets; any residual form blocks."""
    if safety.protected_present(text,safety.protected_needles(values=replacements)):
        raise safety.Blocked('mock diagnostic protected material remains after redaction')


def _discover(root):
    """Bounded walk for vendor databases and logs; vanished entries are skipped (§13)."""
    databases=[];logs=[];entries=0
    def failed(error):
        if isinstance(error,FileNotFoundError):return
        raise safety.Blocked('mock diagnostic discovery unreadable') from error
    for current,directories,files in os.walk(root,onerror=failed):
        directories.sort()
        entries+=len(directories)+len(files)
        if entries>DIAGNOSTIC_ENTRIES:raise safety.Blocked('mock diagnostic discovery bound')
        for name in files:
            if name=='opencode.db':databases.append(Path(current)/name)
            elif name.endswith('.log'):logs.append(Path(current)/name)
        if len(databases)+len(logs)>DIAGNOSTIC_SOURCES:
            raise safety.Blocked('mock diagnostic source bound')
    return sorted(databases),sorted(logs)


def _native_errors(data):
    """Only E34's message error and tool-part state error locations (§13)."""
    if type(data) is not dict:raise safety.Blocked('mock diagnostic message shape unavailable')
    found=[data['error']] if data.get('error') is not None else []
    content=data.get('content',[])
    if type(content) is not list:raise safety.Blocked('mock diagnostic message parts unavailable')
    if len(content)>DIAGNOSTIC_PARTS:raise safety.Blocked('mock diagnostic part bound')
    for part in content:
        state=part.get('state') if type(part) is dict and part.get('type')=='tool' else None
        if type(state) is dict and state.get('error') is not None:found.append(state['error'])
    for error in found:
        if type(error) is not dict:
            yield None,None;continue
        name=error.get('type',error.get('name'))
        details=error.get('data')
        message=error.get('message',details.get('message') if type(details) is dict else None)
        yield name if type(name) is str else None,message if type(message) is str else None


def _boot_directory(line):
    """The exact `directory` field of a location boot line, or None (§13)."""
    match=BOOT_DIRECTORY.search(line)
    if match is None:return None
    token=match.group(1)
    if not token.startswith('"'):return token
    try:value=json.loads(token)
    except ValueError:return None
    return value if type(value) is str else None


def capture(root,ownership,sessions,*,replacements=()):
    """Read only errors/log windows for enrolled, verified mock identities (§13)."""
    from opencode_runroot import _regular, _diagnostic, NATIVE_BYTES
    root=Path(root)
    for sid,row in sessions.items():
        if type(sid) is not str or not re.fullmatch(r'ses_[A-Za-z0-9_]+',sid) \
                or row.get('model')!=safety.MOCK_IDENTITY:
            raise safety.Blocked('mock diagnostic session identity unverifiable')
    rows=[];log_rows=[];total=0;errors=0;input_bytes=0;log_bytes=0
    def safe(text):
        nonlocal total
        value=redact(text,root,replacements);total+=len(value.encode())
        if total>DIAGNOSTIC_BYTES:raise safety.Blocked('mock diagnostic aggregate bound')
        return value
    @_diagnostic
    def read():
        nonlocal errors,input_bytes,log_bytes
        databases,logs=_discover(root)
        for db in databases:
            owned=ownership.classify(db)
            if owned is None or owned.kind!='vendor-private':
                raise safety.Blocked('mock diagnostic database ownership unverifiable')
            _regular(db)
            with sqlite3.connect('file:'+str(db)+'?mode=ro',uri=True,timeout=1) as conn:
                for sid,session in sorted(sessions.items()):
                    # Size and type come first; an oversized body is never fetched.
                    cursor=conn.execute("SELECT seq,typeof(data),octet_length(data),"
                        "CASE WHEN typeof(data)='text' AND octet_length(data)<=? THEN data END "
                        "FROM session_message WHERE session_id=? ORDER BY seq LIMIT ?",
                        (NATIVE_BYTES,sid,DIAGNOSTIC_ROWS+1))
                    found=[];count=0
                    for seq,kind,size,raw in cursor:
                        count+=1
                        if count>DIAGNOSTIC_ROWS:raise safety.Blocked('mock diagnostic row bound')
                        if type(seq) is not int or not 0<=seq<2**63:
                            raise safety.Blocked('mock diagnostic message sequence unverifiable')
                        if kind!='text' or type(size) is not int or size>NATIVE_BYTES or type(raw) is not str:
                            raise safety.Blocked('mock diagnostic message bound')
                        input_bytes+=size
                        if input_bytes>DIAGNOSTIC_INPUT_BYTES:raise safety.Blocked('mock diagnostic input bound')
                        data=json.loads(raw);del raw
                        for name,message in _native_errors(data):
                            errors+=1
                            if errors>DIAGNOSTIC_ROWS:raise safety.Blocked('mock diagnostic error bound')
                            found.append({'seq':seq,'name':safe(name) if name is not None else None,
                                          'message':safe(message) if message is not None else None})
                    if count:
                        fixture=session.get('fixture')
                        rows.append({'sessionID':sid,'model':safety.MOCK_IDENTITY,
                                     'fixture':fixture if type(fixture) is str and len(fixture)<=128 else None,
                                     'errors':found})
        # Scope context lines to an enrolled session or its most recent exact
        # location boot. An explicit foreign session ID always excludes the line.
        locations={str(row['project']) for row in sessions.values() if row.get('project')}
        for path in logs:
            owned=ownership.classify(path)
            if owned is None or owned.kind!='vendor-private':continue
            _regular(path)
            with open(os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC),'rb') as stream:
                info=os.fstat(stream.fileno())
                if not stat.S_ISREG(info.st_mode) or info.st_size>DIAGNOSTIC_LOG_BYTES:
                    raise safety.Blocked('mock diagnostic log bound')
                log_bytes+=info.st_size
                if log_bytes>DIAGNOSTIC_LOGS_BYTES:raise safety.Blocked('mock diagnostic log aggregate bound')
                content=stream.read(info.st_size)  # Later vendor growth is never read.
            source='vendor-log-'+hashlib.sha256(path.relative_to(root).as_posix().encode()).hexdigest()[:16]
            relevant=False
            for number,line in enumerate(content.decode('utf-8','replace').splitlines(),1):
                if 'location services booted' in line:
                    relevant=_boot_directory(line) in locations
                ids=set(re.findall(r'\bses_[A-Za-z0-9_]+',line))
                if ids-sessions.keys():continue
                if not (ids or relevant):continue
                if not (ids or re.search(r'\berror\b',line,re.I)):continue
                log_rows.append({'source':source,'line':number,'text':safe(line),
                                 'association':'session-id' if ids else 'location-context'})
                if len(log_rows)>DIAGNOSTIC_ROWS:raise safety.Blocked('mock diagnostic log row bound')
        record={'mode':'mock-only','sessions':rows,'log_lines':log_rows}
        _check_protected(json.dumps(record),replacements)  # The whole assembled record.
        return record
    return read()
