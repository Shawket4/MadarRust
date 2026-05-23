import os
import re

def extract_context():
    os.makedirs("api_dumps", exist_ok=True)
    
    for root, _, files in os.walk("src"):
        if "routes.rs" not in files or "handlers.rs" not in files:
            continue
            
        module_name = os.path.basename(root)
        if module_name == "src": continue
        
        # 1. Map routes to handlers
        routes_path = os.path.join(root, "routes.rs")
        with open(routes_path, 'r', encoding='utf-8') as f:
            routes_content = f.read()
            
        # Match .route("/path", web::method().to(handlers::func))
        route_pattern = re.compile(r'\.route\("([^"]*)",\s*web::([a-z]+)\(\)\.to\((?:handlers::)?([a-zA-Z0-9_]+)\)\)')
        route_mappings = route_pattern.findall(routes_content)
        
        if not route_mappings:
            continue

        # 2. Extract handler code
        handlers_path = os.path.join(root, "handlers.rs")
        with open(handlers_path, 'r', encoding='utf-8') as f:
            lines = f.readlines()

        dump_content = f"# Module: {module_name}\n\n"
        
        for (path, method, func_name) in route_mappings:
            dump_content += f"### Route: {method.upper()} {path} -> {func_name}\n"
            dump_content += '`' * 3 + 'rust\n'
            
            in_func = False
            brace_count = 0
            
            for line in lines:
                if re.match(rf'^\s*pub\s+async\s+fn\s+{func_name}\b', line):
                    in_func = True
                
                if in_func:
                    dump_content += line
                    brace_count += line.count('{')
                    brace_count -= line.count('}')
                    
                    if brace_count == 0 and ('{' in line or '}' in line):
                        dump_content += '`' * 3 + '\n\n'
                        in_func = False
                        break

        # Save to dump file
        dump_path = os.path.join("api_dumps", f"{module_name}.txt")
        with open(dump_path, 'w', encoding='utf-8') as f:
            f.write(dump_content)
            
    print("✅ Extracted endpoint context into ./api_dumps/")

if __name__ == "__main__":
    extract_context()