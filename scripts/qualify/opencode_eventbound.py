"""§13 closed facts for an owned SSE frame over the C1 §9 1 MiB line bound.

Only closed event types, reviewed key paths and byte counts leave this
module; string values, IDs and every unreviewed dictionary key (`*`) are never
retained.
"""
import json
import re

from opencode_c1 import EVENT_TYPES

# Pinned 2.0.22 native event definitions: every `type:"..."` literal passed to
# the binary's durable (Po) and bus (et) event constructors.
OPENCODE_EVENT_TYPES = frozenset({
    'agent.updated', 'command.executed', 'command.updated', 'config.updated',
    'credential.switched', 'credential.updated', 'file.edited', 'filesystem.changed',
    'form.cancelled', 'form.created', 'form.replied', 'global.disposed',
    'installation.update-available', 'installation.updated', 'integration.updated',
    'location.shutdown', 'lsp.updated', 'mcp.prompts.changed', 'mcp.resources.changed',
    'mcp.status.changed', 'mcp.tools.changed', 'message.part.delta', 'message.part.removed',
    'message.part.updated', 'message.removed', 'message.updated', 'model.updated',
    'models-dev.refreshed', 'permission.asked', 'permission.replied', 'persistent-pty.added',
    'persistent-pty.removed', 'plugin.updated', 'project.updated', 'provider.updated',
    'pty.created', 'pty.deleted', 'pty.exited', 'pty.updated', 'reference.updated',
    'server.connected', 'session.agent.selected', 'session.compacted',
    'session.compaction.delta', 'session.compaction.ended', 'session.compaction.failed',
    'session.compaction.started', 'session.created', 'session.deleted', 'session.diff',
    'session.error', 'session.execution.failed', 'session.execution.interrupted',
    'session.execution.started', 'session.execution.succeeded', 'session.forked',
    'session.idle', 'session.inbox.cancelled', 'session.inbox.delivered',
    'session.inbox.delivery.changed', 'session.inbox.enqueued', 'session.instructions.updated',
    'session.message.content.updated', 'session.metadata.updated', 'session.model.selected',
    'session.moved', 'session.permissions', 'session.reasoning.delta', 'session.reasoning.ended',
    'session.reasoning.started', 'session.renamed', 'session.retry.scheduled',
    'session.revert.cleared', 'session.revert.committed', 'session.revert.staged',
    'session.shell.ended', 'session.shell.started', 'session.skill.activated', 'session.status',
    'session.step.ended', 'session.step.failed', 'session.step.started', 'session.step.streamed',
    'session.synthetic', 'session.text.delta', 'session.text.ended', 'session.text.started',
    'session.tool.called', 'session.tool.failed', 'session.tool.input.delta',
    'session.tool.input.ended', 'session.tool.input.started', 'session.tool.progress',
    'session.tool.success', 'session.updated', 'session.usage.recorded', 'session.usage.updated',
    'session.viewed', 'shell.created', 'shell.deleted', 'shell.exited', 'skill.updated',
    'tui.command.execute', 'tui.prompt.append', 'tui.session.select', 'tui.toast.show',
    'vcs.branch.updated', 'websearch.updated', 'workspace.failed', 'workspace.ready',
    'workspace.status', 'worktree.failed', 'worktree.ready', 'worktree.resolved',
    'worktree.updated'})
KNOWN_TYPES = OPENCODE_EVENT_TYPES | EVENT_TYPES
FIXTURE_RUN_BYTES = 64 * 1024  # Near-limit fixture prompts are 'x' runs far above this.
FIELD_MIN_BYTES = 512  # Smaller subtrees are summed into their parent.
FIELD_DEPTH = 8
FIELD_ROWS = 128
_RUN = re.compile(r'x{%d,}' % FIXTURE_RUN_BYTES)
_RUN_BYTES = re.compile(rb'x{%d,}' % FIXTURE_RUN_BYTES)
_TYPE = re.compile(rb'"type"\s*:\s*"([a-z][a-z0-9._-]{0,63})"')


# Reviewed schema paths: the fields VIA's own 2.0.22 event decoder reads
# (crates/via-routes/src/opencode/events.rs) plus `session.step.ended` `files`,
# recorded by an earlier bound failure (§13). Every other key is `*`.
_ENVELOPE = frozenset({'id', 'type', 'durable', 'durable.seq', 'data', 'data.sessionID'})
_TOKENS = ('tokens', 'tokens.input', 'tokens.output', 'tokens.reasoning', 'tokens.cache',
           'tokens.cache.read', 'tokens.cache.write')
_ERROR = ('error', 'error.type', 'error.status')
_STEP = ('assistantMessageID', 'finish', 'cost', 'files', *_TOKENS, *_ERROR)
_TEXT = ('assistantMessageID', 'ordinal', 'delta', 'text')
_TOOL = ('assistantMessageID', 'id', 'name', *_ERROR)
_DATA = {
    **{kind: ('inboxID',) for kind in ('session.inbox.enqueued', 'session.inbox.delivered',
                                       'session.inbox.cancelled', 'session.inbox.delivery.changed')},
    'session.execution.started': (), 'session.execution.succeeded': (),
    'session.execution.failed': _ERROR, 'session.execution.interrupted': ('reason',),
    **{kind: _STEP for kind in ('session.step.started', 'session.step.ended', 'session.step.failed',
                                'session.step.streamed')},
    **{kind: _TEXT for kind in ('session.text.started', 'session.text.delta', 'session.text.ended',
                                'session.reasoning.started', 'session.reasoning.delta',
                                'session.reasoning.ended')},
    **{kind: _TOOL for kind in ('session.tool.input.started', 'session.tool.called', 'session.tool.success',
                                'session.tool.failed', 'session.tool.input.ended', 'session.tool.progress')},
    **{kind: ('inputID', 'cost', *_TOKENS, *_ERROR) for kind in ('session.compaction.ended',
                                                                 'session.compaction.failed')},
    'permission.asked': ('id', 'action', 'source', 'source.messageID', 'source.id'),
    'permission.replied': ('requestID',),
    'form.cancelled': ('id',), 'form.replied': ('id',),
    'form.created': ('form', 'form.id', 'form.sessionID'),
    'session.created': ('parentID',),
}
REVIEWED_PATHS = {kind: _ENVELOPE | {'data.' + path for path in paths} for kind, paths in _DATA.items()}


def _key(reviewed, parent, name):
    return name if (parent + '.' if parent else '') + name in reviewed else '*'


def _json_bytes(value):
    return len(json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode())


def _longest(pattern, value):
    return max((len(match) for match in pattern.findall(value)), default=0)


def event_bound_facts(payload):
    """Closed type, key-path byte sizes and fixture-prompt field sizes of one SSE data payload."""
    facts = {'event_type': 'unparsed', 'payload_bytes': len(payload),
             'payload_x_run_bytes': _longest(_RUN_BYTES, payload),
             'type_literals': [], 'fixture_fields': [], 'field_bytes': []}
    try:
        value = json.loads(payload)
    except (ValueError, RecursionError):
        facts['type_literals'] = sorted({found.decode() for found in _TYPE.findall(payload)
                                         if found.decode() in KNOWN_TYPES})
        return facts
    kind = value.get('type') if type(value) is dict else None
    facts['event_type'] = kind if type(kind) is str and kind in KNOWN_TYPES else 'other'
    reviewed = REVIEWED_PATHS.get(facts['event_type'], _ENVELOPE)
    sizes = {}
    stack = [(value, '', 0)]
    while stack:
        node, path, depth = stack.pop()
        if type(node) is str:
            run = _longest(_RUN, node)
            if run:
                facts['fixture_fields'].append({'path': path, 'field_bytes': len(node.encode()),
                                                'x_run_bytes': run, 'equals': run == len(node)})
        if type(node) not in (dict, list, str):
            continue
        if depth <= FIELD_DEPTH:
            # A subtree under the floor cannot hold a fixture run either.
            size = _json_bytes(node)
            if size < FIELD_MIN_BYTES:
                continue
            row = sizes.setdefault(path or '$', {'path': path or '$', 'json_bytes': 0, 'count': 0})
            row['json_bytes'] += size; row['count'] += 1
        if type(node) is dict:
            stack.extend((item, (path + '.' if path else '') + _key(reviewed, path, name), depth + 1)
                         for name, item in node.items())
        elif type(node) is list:
            stack.extend((item, path + '[]', depth + 1) for item in node)
    facts['fixture_fields'].sort(key=lambda row: (-row['field_bytes'], row['path']))
    facts['field_bytes'] = sorted(sizes.values(), key=lambda row: (-row['json_bytes'],
                                                                   row['path']))[:FIELD_ROWS]
    return facts
