# Shared API test examples

`fixtures.json` contains ordinary JSON examples shared by Rust and Python tests. It is test data, not part of the API specification or generated types.

Keep successful requests, producer replies and reader edge cases here when several tests or languages use them. Every `*_cases` key is checked against the JSON Schemas by `tests/test_api_contract.py` (`test_fixture_cases_match_the_schemas`), which lists how each key is checked. Use concise JSON records and short arrays; no custom expansion or fixture loader is required.

Most malformed-input corpora keep a representative rejection rather than a wrong-type matrix. Where cases name distinct public endpoints, methods or schemas, one rejection remains per combination. Focused shared suites additionally cover descriptor shape, mailbox UTF-8 byte and integer boundaries, attachment completions, inbox core fields and directional phase gates. Tests beside the implementation retain their independent failure, ownership and cleanup assertions.

Add a focused regression example when it demonstrates missing coverage. Avoid enumerating every wrong scalar type or duplicating a complete payload for each generated field. Prefer a small local value or a modification of an existing example in the test when only one test needs it.
