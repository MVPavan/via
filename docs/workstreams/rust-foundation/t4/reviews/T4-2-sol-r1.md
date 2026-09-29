**UNSOUND**

## Findings

- **Important — `crates/via-store/src/blob.rs:116`:** A timeout drops the `spawn_blocking` join handle while the file operation continues. Repeated large-prompt requests on a stalled filesystem can leave blocking tasks accumulating with no owner to observe or join them, contrary to `.repo-context/coding-style.md:108`. Keep timed-out work in a Store-owned, bounded tracker until it completes. The next-startup sweep for an unreferenced file is acceptable; losing task ownership is the defect.
- **Important — `crates/via-store/src/blob.rs:372`:** Cancelling a request can run `BlobWriter::drop` on a Tokio worker, where it calls `remove_file` synchronously and without a deadline. If unlink stalls, that worker cannot serve other requests or deadlines. Route this cleanup through the owned blocking path.

I checked the remaining T4-2 Sol scrutiny points: lane service and wakeups, reply drops after unlocking, writer death and in-flight replies, Public refusal mapping, exhaustive command sizing and atomic batches, blob creation and durability order, and JSON string and escape counting. The reported xorshift test, text blob reference, extra test seams, cross-path edits, deferred C1 envelope reads, and expected T4-3 merge conflict are not findings.

## Measurement suggestions

Measure blob step timeouts and outstanding blocking jobs after the ownership fix; measure per-lane wait time before changing lane priorities.

## Could not verify

I did not reproduce a stalled filesystem or rerun the full Gate G. The T4-2 selector passed **10/10**, the focused C1 parse test passed **1/1**, and feature-enabled clippy, format, layer, and diff checks passed. Git status remained clean.