"""Explicit mock-only error diagnostics, never public-session content (§13)."""
import base64
import json
from pathlib import Path
import re
import sqlite3
import urllib.parse

import opencode_safety as safety

# §13: only cases whose model-capable operations are scripted loopback requests.
MOCK_CASES={'usage':'ledger','compaction':'ledger'}
DIAGNOSTIC_BYTES=1024*1024  # §13: error/log text is bounded separately from payloads.
DIAGNOSTIC_ROWS=1024  # §13: only enrolled mock-session failures, no database dump.


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
    """Remove private-root and all protected representations before the sink (§13)."""
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
    return raw.decode('utf-8','replace')


def capture(root,ownership,sessions,*,replacements=()):
    """Read only errors/log windows for enrolled, verified mock identities (§13)."""
    from opencode_runroot import _regular, _diagnostic
    root=Path(root)
    for sid,row in sessions.items():
        if type(sid) is not str or not re.fullmatch(r'ses_[A-Za-z0-9_]+',sid) \
                or row.get('model')!=safety.MOCK_IDENTITY:
            raise safety.Blocked('mock diagnostic session identity unverifiable')
    rows=[];log_rows=[];total=0
    def safe(text):
        nonlocal total
        value=redact(text,root,replacements);total+=len(value.encode())
        if total>DIAGNOSTIC_BYTES:raise safety.Blocked('mock diagnostic aggregate bound')
        return value
    def errors(value,depth=0):
        if depth>12:raise safety.Blocked('mock diagnostic error depth')
        if type(value) is dict:
            for key,part in value.items():
                if key=='error' and type(part) is dict:
                    name=part.get('type',part.get('name'))
                    details=part.get('data',{})
                    message=part.get('message',details.get('message') if type(details) is dict else None)
                    yield {'name':safe(name) if type(name) is str else None,
                           'message':safe(message) if type(message) is str else None}
                elif type(part) in {dict,list}:yield from errors(part,depth+1)
        elif type(value) is list:
            for part in value:yield from errors(part,depth+1)
    @_diagnostic
    def read():
        for db in sorted(root.rglob('opencode.db')):
            owned=ownership.classify(db)
            if owned is None or owned.kind!='vendor-private':
                raise safety.Blocked('mock diagnostic database ownership unverifiable')
            _regular(db)
            with sqlite3.connect('file:'+str(db)+'?mode=ro',uri=True,timeout=1) as conn:
                for sid,session in sorted(sessions.items()):
                    messages=conn.execute('SELECT seq,data FROM session_message WHERE session_id=? ORDER BY seq',
                                          (sid,)).fetchmany(DIAGNOSTIC_ROWS+1)
                    if len(messages)>DIAGNOSTIC_ROWS:raise safety.Blocked('mock diagnostic row bound')
                    found=[]
                    for seq,raw in messages:
                        if type(raw) is not str or len(raw.encode())>DIAGNOSTIC_BYTES:
                            raise safety.Blocked('mock diagnostic message bound')
                        found.extend({'seq':seq,**item} for item in errors(json.loads(raw)))
                    if messages:rows.append({'sessionID':sid,'model':safety.MOCK_IDENTITY,
                                             'fixture':session.get('fixture'),'errors':found})
        # Scope context lines to an enrolled session or its most recent location
        # boot. An explicit foreign session ID always excludes the line.
        locations={str(row['project']) for row in sessions.values() if row.get('project')}
        for path in sorted(root.rglob('*.log')):
            owned=ownership.classify(path)
            if owned is None or owned.kind!='vendor-private':continue
            if _regular(path).st_size>16*DIAGNOSTIC_BYTES:raise safety.Blocked('mock diagnostic log bound')
            relevant=False
            for number,line in enumerate(path.read_text(errors='replace').splitlines(),1):
                if 'location services booted' in line:
                    relevant=any(location in line for location in locations)
                ids=set(re.findall(r'\bses_[A-Za-z0-9_]+',line))
                if ids-sessions.keys():continue
                if not (ids or relevant):continue
                if not (ids or re.search(r'\berror\b',line,re.I)):continue
                log_rows.append({'source':path.relative_to(root).as_posix(),'line':number,'text':safe(line)})
                if len(log_rows)>DIAGNOSTIC_ROWS:raise safety.Blocked('mock diagnostic log row bound')
        return {'mode':'mock-only','sessions':rows,'log_lines':log_rows}
    return read()
