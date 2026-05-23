import re
import glob

# 1. Find all actix routes
actix_routes = set()
for file in glob.glob("src/**/routes.rs", recursive=True):
    with open(file) as f:
        content = f.read()
        matches = re.findall(r'web::(?:get|post|put|delete|patch)\(\)\.to\(handlers::([a-zA-Z0-9_]+)\)', content)
        actix_routes.update(matches)

# 2. Find all registered in openapi.rs
openapi_routes = set()
with open("src/openapi.rs") as f:
    content = f.read()
    matches = re.findall(r'crate::[a-zA-Z0-9_]+::handlers::([a-zA-Z0-9_]+)', content)
    openapi_routes.update(matches)

# 3. Print
print("Actix Routes:", len(actix_routes))
print("OpenAPI Routes:", len(openapi_routes))
print("Missing from OpenAPI:")
for r in sorted(actix_routes - openapi_routes):
    print(" -", r)
