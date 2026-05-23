import os
import re
import json

def analyze_rust_project(src_dir="src"):
    project_meta = {
        "models": [],
        "routes": []
    }
    
    # Regex patterns
    struct_pattern = re.compile(r'pub\s+(?:struct|enum)\s+(\w+)')
    actix_pattern = re.compile(r'#\[(get|post|put|delete|patch)\("([^"]+)"\)\]')
    fn_pattern = re.compile(r'pub\s+async\s+fn\s+(\w+)')

    for root, _, files in os.walk(src_dir):
        for file in files:
            if not file.endswith(".rs"):
                continue
                
            file_path = os.path.relpath(os.path.join(root, file))
            
            with open(os.path.join(root, file), 'r', encoding='utf-8') as f:
                content = f.read()
                
            # Extract Schemas/Models
            structs = struct_pattern.findall(content)
            if structs:
                project_meta["models"].append({
                    "file": file_path,
                    "module": file_path.replace("src/", "").replace(".rs", "").replace("/", "::"),
                    "structs": structs
                })
                
            # Extract Actix Routes
            # Find all locations of actix attributes and the function immediately following them
            lines = content.split('\n')
            for i, line in enumerate(lines):
                route_match = actix_pattern.search(line)
                if route_match:
                    method, path = route_match.groups()
                    # Look ahead a few lines to find the async fn name
                    fn_name = None
                    for j in range(i + 1, min(i + 6, len(lines))):
                        fn_match = fn_pattern.search(lines[j])
                        if fn_match:
                            fn_name = fn_match.group(1)
                            break
                    
                    if fn_name:
                        project_meta["routes"].append({
                            "file": file_path,
                            "module": file_path.replace("src/", "").replace(".rs", "").replace("/", "::"),
                            "method": method,
                            "path": path,
                            "handler": fn_name
                        })

    return project_meta

if __name__ == "__main__":
    if not os.path.exists("src"):
        print("Error: Could not find 'src' directory. Please run this from your Rust project root.")
    else:
        metadata = analyze_rust_project()
        # Pretty print the final structure
        print(json.dumps(metadata, indent=2))