The hidden tests are generated from node-semver, so the expected answers come
from the package whose rules the prompt states rather than from the reference.

```console
npm install semver@7.6.3
node oracle.js            # writes oracle.json
python3 gen.py oracle.json ../hidden/tests
```

Then check the reference still passes every hidden test.
