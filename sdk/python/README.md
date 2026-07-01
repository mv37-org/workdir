# mv37-workdir

Python SDK for [workdir](https://workdir.dev).

```bash
pip install mv37-workdir
```

```python
import time
from workdir import Client

workdir = Client("https://api.workdir.dev", api_key="...")

box = workdir.sandboxes.create()
print(box.exec("echo hello").stdout)

job = box.exec("pytest", background=True)
status = box.exec_status(job.cmd_id)
while status.state == "running":
    time.sleep(1)
    status = box.exec_status(job.cmd_id)
print(box.exec_logs(job.cmd_id))
box.delete()
```

The SDK uses only the Python standard library.
