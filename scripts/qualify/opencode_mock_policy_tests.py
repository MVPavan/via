"""Seams-phase mock ceiling follows each case's request pattern (§13 mock admission)."""
import ast
import collections
import inspect
import textwrap
import unittest

import opencode_cases as cases
import opencode_mock_policy as policy

# Driver operations that can make a mock model request or retire a generation.
MODEL_OPERATIONS = frozenset({'start_turn', 'near_limit_inbox', 'idle_retirement', 'foreign_prompt'})


def operation_signature(function):
    """Count model-capable call sites, multiplied by enclosing literal loop bounds."""
    if function.__name__ == '<lambda>':
        # Registry lambdas may only delegate to the request-free deferral.
        if set(function.__code__.co_names) != {'case_deferred'}:
            raise AssertionError('seams registry lambda is not a deferral')
        return {}
    tree = ast.parse(textwrap.dedent(inspect.getsource(function)))
    counts = collections.Counter()

    def bound(loop):
        if isinstance(loop.iter, (ast.Tuple, ast.List)):
            return len(loop.iter.elts)
        if isinstance(loop.iter, ast.Call) and getattr(loop.iter.func, 'id', None) == 'range' \
                and len(loop.iter.args) == 1 and isinstance(loop.iter.args[0], ast.Constant):
            return loop.iter.args[0].value
        raise AssertionError('seams case loop bound is not a literal')

    def visit(node, multiplier):
        if isinstance(node, ast.While):
            raise AssertionError('seams case has an unbounded loop')
        if isinstance(node, ast.For):
            for child in node.body + node.orelse:
                visit(child, multiplier * bound(node))
            return
        if isinstance(node, ast.Call):
            name = getattr(node.func, 'id', None)
            if name in {'turn', 'fixture'}:
                counts[name] += multiplier
            if getattr(node.func, 'attr', None) == 'execute' and node.args \
                    and isinstance(node.args[0], ast.Constant) and node.args[0].value in MODEL_OPERATIONS:
                counts[node.args[0].value] += multiplier
        for child in ast.iter_child_nodes(node):
            visit(child, multiplier)

    for statement in tree.body[0].body:
        visit(statement, 1)
    return dict(counts)


class SeamsCeilingTests(unittest.TestCase):
    def test_every_seams_case_pattern_matches_its_declared_budget(self):
        model_capable = {name for name in cases.PHASE_CASES['seams']
                         if operation_signature(cases.CASES[name])}
        self.assertEqual(set(policy.SEAMS_CASE_OPERATIONS), model_capable)
        self.assertEqual(set(policy.SEAMS_CASE_REQUESTS), model_capable)
        for name in model_capable:
            with self.subTest(case=name):
                self.assertEqual(operation_signature(cases.CASES[name]),
                                 policy.SEAMS_CASE_OPERATIONS[name])

    def test_seams_phase_ceiling_is_the_derived_sum(self):
        derived = policy.SEAMS_INITIAL_ACQUISITION + policy.REQUEST_MARGIN + sum(
            row['own'] + row['reacquisitions'] * policy.BOOTSTRAP_REQUESTS
            for row in policy.SEAMS_CASE_REQUESTS.values())
        self.assertEqual(policy.SEAMS_PHASE_REQUESTS, derived)
        seams = next(row for row in cases.PHASES if row.name == 'seams')
        self.assertEqual(seams.mock_turns, policy.SEAMS_PHASE_REQUESTS)

    def test_deferred_seams_rows_make_no_model_request(self):
        for name in ('collision', 'forms', 'other'):
            with self.subTest(case=name):
                self.assertEqual(operation_signature(cases.CASES[name]), {})


if __name__ == '__main__':
    unittest.main()
