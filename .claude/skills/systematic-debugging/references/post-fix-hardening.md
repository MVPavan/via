# Post-fix hardening — defense in depth

Load after the fix, when the root cause was invalid data or wrong state
crossing several layers. One validation says "we fixed the bug"; a check at
every layer the value passes through says "the bug is structurally
impossible" — alternate code paths, refactors, and mocks bypass a single check.

## The four layers

| Layer | Purpose | Shape |
| --- | --- | --- |
| Entry validation | reject invalid input at the public boundary | raise on empty/missing/nonsense arguments |
| Business-logic validation | ensure the value makes sense for this operation | precondition checks inside the operation |
| Environment guard | forbid dangerous operations in the wrong context | refuse destructive filesystem/git/store ops outside a temp dir under test |
| Forensic logging | capture context for the case the other layers miss | log value + caller context before the dangerous operation |

In this repo, entry validation is usually a frozen pydantic model — put the
constraint on the field so the bad value cannot be constructed at all.

```python
def create_project(name: str, working_directory: Path) -> Project:
    if not working_directory.is_dir():  # entry validation
        raise ValueError(f"working_directory must be an existing directory: {working_directory!r}")
    ...

def git_init(directory: Path, settings: AppSettings) -> None:
    # environment guard — settings injected at construction; never read os.environ here
    if settings.env is Env.TEST and not directory.is_relative_to(tempfile.gettempdir()):
        raise RuntimeError(f"refusing git init outside temp dir under test: {directory}")
    ...
```

## Applying it

1. Trace the bad value's full path — caller by caller from the point of
   damage back to its origin.
2. Map every checkpoint the value passes through.
3. Add the appropriate check at each layer.
4. **Bypass-test each layer**: defeat layer 1 deliberately and verify layer 2
   catches it, and so on down. In the session this pattern comes from, every
   layer caught bypasses the others missed — mocks skipped business checks,
   alternate code paths skipped entry validation.

Scope the layers to the bug class just fixed — this hardens a proven failure
path, it is not license for speculative validation everywhere (the simplicity
rules still apply).
