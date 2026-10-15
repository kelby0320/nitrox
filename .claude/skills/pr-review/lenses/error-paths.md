# Lens: error paths and partial state

Code right on the happy path and wrong when its second step fails is where this project's recent
bugs have been.

For each operation under review that changes state in more than one step — writes a disk, a
table and a count; sends, then records; allocates, then links:

1. List the steps and, for each, what is true if it fails there: what was written, what was
   freed, what still points at what.
2. Ask whether the rest of the system can live with that state: a tree that still reads, a count
   that still matches, a handle still owned, no block both free and in use, no waiter waiting
   forever.
3. Demonstrate with the crate's failure injection where it has one — a writer that fails the nth
   write, or every write from the nth — or with a probe that returns an error at the step. Fail each
   step in turn: a sweep finds more than one case.

Also: a result discarded with `let _ =` where it matters, a retry with no bound, a cleanup that
can run twice, and a resource released on one path and not on its sibling.
