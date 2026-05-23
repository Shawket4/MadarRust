import re
import os

actix_handlers = set()
for root, _, files in os.walk('src'):
    for f in files:
        if f.endswith('.rs'):
            with open(os.path.join(root, f)) as file:
                content = file.read()
                matches = re.findall(r'\.to\((?:crate::[a-z_]+::handlers::|handlers::)?([a-z_]+)\)', content)
                for m in matches:
                    actix_handlers.add(m)

openapi_handlers = set()
with open('src/openapi.rs') as f:
    content = f.read()
    matches = re.findall(r'crate::[a-z_]+::handlers::([a-z_]+)', content)
    for m in matches:
        openapi_handlers.add(m)

missing = sorted(actix_handlers - openapi_handlers)
for m in missing:
    print(m)
