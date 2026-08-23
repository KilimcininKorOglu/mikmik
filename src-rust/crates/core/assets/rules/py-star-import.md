---
description: Never use a star import; name what you import
condition: "from\\s+[\\w.]+\\s+import\\s+\\*"
scope: "tool:Edit(*.py), tool:Write(*.py)"
---

`from x import *` puts an unknown set of names into the module, decided by the
other module's contents at import time. A reader cannot tell where a name came
from, a linter cannot tell whether it is used, and a later release of `x` can
shadow one of your own names without any change on your side.

## Avoid

```python
from os.path import *
from .models import *
```

## Use

```python
from os.path import join, dirname
from .models import User, Session
```

For a module you use widely, import the module itself:

```python
import numpy as np
```
