import os
import re

def process_file(filepath):
    with open(filepath, 'r') as f:
        content = f.read()

    # Skip if already imported ToSchema
    if 'use utoipa::ToSchema;' in content:
        return

    # Find all structs and enums
    # We want to add #[derive(ToSchema)] to them, or add ToSchema to existing derives
    
    modified = False
    
    # 1. Add utoipa::ToSchema import at the top if there are structs/enums
    if re.search(r'pub (?:struct|enum)', content):
        content = 'use utoipa::ToSchema;\n' + content
        modified = True

    # 2. Add #[derive(ToSchema)] to structs and enums
    # This regex matches pub struct or pub enum that might have attributes above it.
    # Actually, the simplest way is to look for #[derive(...)] and insert ToSchema, 
    # OR if no derive exists, add it.
    
    # Find all `pub struct` or `pub enum`
    lines = content.split('\n')
    new_lines = []
    
    for i, line in enumerate(lines):
        if 'pub struct ' in line or 'pub enum ' in line:
            # Check previous line for derive
            if i > 0 and '#[derive(' in lines[i-1] and 'ToSchema' not in lines[i-1]:
                lines[i-1] = lines[i-1].replace('#[derive(', '#[derive(ToSchema, ')
                modified = True
            elif i > 0 and '#[derive(ToSchema' in lines[i-1]:
                pass # Already has it
            else:
                # Add derive above
                new_lines.append('#[derive(ToSchema)]')
                modified = True
        new_lines.append(line)
        
    if modified:
        with open(filepath, 'w') as f:
            f.write('\n'.join(new_lines))
        print(f"Annotated {filepath}")

def main():
    src_dir = os.path.join(os.path.dirname(__file__), 'src')
    for root, dirs, files in os.walk(src_dir):
        for file in files:
            if file.endswith('.rs'):
                process_file(os.path.join(root, file))

if __name__ == '__main__':
    main()
