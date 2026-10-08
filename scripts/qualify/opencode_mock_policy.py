"""Pinned mock response recipes and bounds (OpenCode packet §13 L4/L7)."""

COMPACTION_TEMPLATE_OFFSET = 145633783
# §13 L4: pinned primary 1+10 retries, selected/fallback title, bootstrap,
# and a fixed two-request allowance. Every physical request still counts.
TITLE_ATTEMPTS = 2
BOOTSTRAP_REQUESTS = 2
REQUEST_MARGIN = 2
HOSTILE_PRIMARY_ATTEMPTS = {'error':11,'malformed':1,'oversized':1,'truncated':11}
HOSTILE_PHASE_REQUESTS = 2 * sum(HOSTILE_PRIMARY_ATTEMPTS[name] + TITLE_ATTEMPTS
    + BOOTSTRAP_REQUESTS + REQUEST_MARGIN for name in ('error','malformed','oversized','truncated'))
NATIVE_SSE_PENDING_BYTES = 10485760  # §13 L4: pinned upstream cap at byte 144448787.

# §13 L14: every implemented fresh point, including optional LSP/reload.
ANCHOR_FRESH_POINTS = ('publication','tool_during','tool_after','location_shell_during',
    'location_shell_after','session_shell_during','session_shell_after','mcp_during',
    'mcp_after','plugin_during','plugin_after','lsp_during','lsp_after','reload')
ANCHOR_POINT_REQUESTS = 2 + 2 + 2  # Initial bootstrap, primary/title, successor bootstrap.
ANCHOR_EXTRA_STEPS = 3  # tool_after and the two configured LSP reads.
ANCHOR_RETAINED_SUCCESSOR = 2
ANCHOR_COLD_CAPABILITIES = 6  # Bootstrap plus read-probe primary/title calls.
ANCHOR_PHASE_REQUESTS = (len(ANCHOR_FRESH_POINTS) * ANCHOR_POINT_REQUESTS
    + ANCHOR_EXTRA_STEPS + ANCHOR_RETAINED_SUCCESSOR + ANCHOR_COLD_CAPABILITIES + REQUEST_MARGIN)


def hostile_budget(response):
    """Per-provider finite attempts, auxiliary work and fixed margin (§13 L4)."""
    primary = HOSTILE_PRIMARY_ATTEMPTS[response]
    return {'requests':primary + TITLE_ATTEMPTS + BOOTSTRAP_REQUESTS + REQUEST_MARGIN,
            'primary_attempts':primary,'title_attempts':TITLE_ATTEMPTS,
            'bootstrap_requests':BOOTSTRAP_REQUESTS,'fixed_margin':REQUEST_MARGIN}
COMPACTION_SUMMARY = """## Objective
- Complete the local mock fixture.

## Requirements
- Use only the local fixture provider.

## Decisions
- (none)

## Work State
### Completed
- Earlier local fixture messages were observed.
### Active
- Answer the latest local fixture request.
### Blocked
- (none)

## Next Move
1. Answer the latest local fixture request.

## Relevant Files
- (none)

## Important Context
- This is a synthetic fixture summary.
"""


def compaction_request(messages):
    """Recognize the pinned summary prompt, never consume a tool recipe (§13 L7)."""
    if type(messages) is not list or not messages:
        return False
    last = messages[-1]
    if type(last) is not dict or last.get('role') != 'user':
        return False
    content = last.get('content')
    if type(content) is list:
        content = '\n'.join(part['text'] for part in content if type(part) is dict
                            and part.get('type') == 'text' and type(part.get('text')) is str)
    return type(content) is str and any(marker in content for marker in (
        'You MUST summarize the conversation above into a structured summary',
        'Update the existing checkpoint in the conversation above into one consolidated summary')) \
        and all(marker in content for marker in (
            'You MUST use this format for your response', '## Objective', '## Work State'))
