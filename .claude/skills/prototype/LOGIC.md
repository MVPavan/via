# Logic prototype

Use a small script or REPL-driven module when execution and assertions answer the
question directly. Use a self-contained HTML demo when a non-developer needs to
manipulate states and inspect outcomes. Neither format is mandatory for every
logic question.

Keep the domain logic readable apart from presentation. Model explicit actions,
state and transitions where appropriate; pure functions, reducers or a small
stateful module are all valid. Surface the relevant state and invalid transitions.

For HTML, include the question, a readable state panel, controls and only the
guided scenarios needed to expose the uncertainty. Reset scenarios to known state.
Use domain labels, keyboard-accessible controls and an offline artifact when
sharing is the goal. For scripts, document the run command and expected signal.

Exercise a representative path and the counterexample the design might mishandle.
Use assertions when they provide inexpensive evidence. Capture the answer and
limits; production implementation follows normal authorization and verification.
