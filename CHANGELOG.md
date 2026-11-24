# btrealloc changelog

## 0.1.0

First release!

- Supports scanning a whole filesystem, a directory tree, or a single file.
- Finds extents with *unreachable* space.
- Reallocates the extents by creating an O_TMPFILE file, and FIDEDUPERANGE the live ranges into it.
