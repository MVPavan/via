SOUND

Findings: none. The reader uses a nonblocking bounded queue, and design §8.5 explicitly says a latched failure drops queued messages. The pacing preserves the byte and order assertions, the huge-line failure and saved-prefix checks, and the separate 1,025th-message overflow check. The other flood tests reviewed do not show the same unpaced pattern.

The three focused via-wire tests passed. The supplied stress logs show 240/240 passes for each changed test after the fix. Git status is clean.

Could not verify: I did not independently rerun the full workspace gate or stress campaign; I inspected their logs.