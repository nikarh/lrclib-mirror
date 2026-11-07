# Security

lrclib-mirror serves public data read-only and has no authentication of its own. Security issues are ways to make it
do more than that: write or delete anything beyond its data directory, publish or change lyrics, make it fetch
something other than the configured dump listing and dumps, run code through a crafted dump or request, or break out
of its restrictions in the container.

Issues of the upstream projects ([lrclib](https://github.com/tranxuanthang/lrclib),
[lrclib-homepage](https://github.com/tranxuanthang/lrclib-homepage)) belong there, unless the way this project uses
them causes them.

Please report them privately, through
[GitHub's vulnerability reporting](https://github.com/nikarh/lrclib-mirror/security/advisories/new), not in a public
issue.
