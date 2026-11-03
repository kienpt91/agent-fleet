These bodies follow the documented shape of the GitHub REST API (issues, pulls, check runs, commit
status, reviews), cut down to a few fields and with extra fields kept where the real answers have
them, so that the readers must ignore what they do not use. They were written by hand from that
documentation, not recorded from a live account: nothing here proves how a real `gh` and a real
GitHub answer today (see the README of the repository, "What was not verified").

Text that has the shape of a secret is never stored in a fixture: a placeholder such as
`@@TOKEN@@` stands for it and the test that reads the fixture builds the value at run time (the
repository's secret scan would rightly refuse it otherwise).
