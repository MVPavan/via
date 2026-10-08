"""Closed qualification vocabulary from C1 §§1,3,5–8; unknowns prove nothing."""

TERMINAL_STATES = frozenset({'completed','failed','cancelled','unknown'})
SCOPES = frozenset({'turn','vendor_interval','session_cumulative'})
STOP_REASONS = frozenset({'end_turn','max_steps','budget','refusal','interrupted','deadline','error','other'})
DENIAL_KINDS = frozenset({'file_write','command','network','other'})
CANCEL_OUTCOMES = frozenset({'requested','acknowledged','forced','unknown'})
CLEANUP_STATES = frozenset({'quiescent','uncertain','pending'})
FAILURE_CLASSES = frozenset({'deadline_wall','deadline_idle','submit_failed','resume_mismatch',
    'vendor_error','rate_limit','auth','context_exceeded','budget_exceeded','server_lost',
    'process_exited','protocol','overflow','structured_output_invalid','daemon_restart','store'})
WARNING_CODES = frozenset({'instructions_partial','vendor_version_untested',
    'usage_interval_unverified','structured_output_missing','structured_output_invalid',
    'cancel_cleanup_uncertain','predecessor_cleanup_uncertain','config_switch_unverified',
    'deprecated','observations_lost','credential_state_unchecked','vendor_passthrough'})
EVENT_TYPES = frozenset({'session.opened','session.closed','session.reopened',
    'turn.queued','turn.submitted','turn.started','turn.ended','turn.revised',
    'action.denied','vendor.request_declined','steer.delivered','cancel.requested',
    'cancel.settled','warning','process.exited','server.lost'})
ERROR_CODES = dict(zip(('parse_error','invalid_request','method_not_found','invalid_params'),
                      (-32700,-32600,-32601,-32602)))
ERROR_CODES.update({name:-32000-index for index,name in enumerate((
    'handshake_required','version_mismatch','invalid_handle','session_not_found',
    'session_closed','turn_not_found','unsupported_verb','missing_capability',
    'bound_unsupported','harness_unavailable','unknown_model','queue_full',
    'admission_refused','no_active_turn','turn_mismatch','turn_not_finished','wait_timeout',
    'daemon_stopping','store_error','history_pruned','request_too_large','steer_failed'))})
