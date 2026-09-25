# UI prototype

Choose a single mockup when testing one interaction; build variants only when
comparing alternatives is the question. Reuse project components and realistic
content density. Keep production authentication, permissions and data handling
intact.

Prefer an isolated preview or scratch artifact. An existing-page preview can add
useful context when editing that page is authorized and the preview cannot leak
into production. Do not create a live route or modify a production page merely
because it is nearby.

For multiple variants, vary meaningful structure or behavior rather than color
alone. A URL selector or small switcher can help comparisons; reuse its logic and
label variants clearly. Keyboard controls must not steal input from editable
fields, and focus, contrast and interaction states remain usable.

Use safe fixtures or scoped read-only data. Stub real mutations unless integration
is the authorized subject of the experiment. Verify the central interaction and
show the preview/file pointer. Record the selected direction or remaining
uncertainty; promotion and removal of production code are separate authorized work.
