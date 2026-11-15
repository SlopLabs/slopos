# Not yet in the docs

- A `command` word may carry `+` (`c++`, `clang++`); an `app` or unix socket
  name may not.
- A declared command may be a symlink in `bin/`: it is delivered when it
  resolves, through the image's own links and at most 7 of them, to an
  executable regular file of the image (`Image::resolved`). A service's, an
  action's or a `program` principal's program is still a regular file at the
  path named.
- `Declaration::programs` no longer lists the commands.
