import os, re

actix_routes = set()
for root, _, files in os.walk('src'):
    for file in files:
        if file.endswith('routes.rs') or file == 'main.rs':
            with open(os.path.join(root, file)) as f:
                content = f.read()
                matches = re.findall(r'\.to\([^\)]*::([a-zA-Z0-9_]+)\)', content)
                actix_routes.update(matches)
                matches2 = re.findall(r'\.to\(([a-zA-Z0-9_]+)\)', content)
                actix_routes.update(matches2)

openapi_routes = set()
with open('src/openapi.rs') as f:
    matches = re.findall(r'crate::[a-zA-Z0-9_:]+::([a-zA-Z0-9_]+)', f.read())
    openapi_routes.update(matches)

# Exclude menu_advisor
menu_advisor = {'get_run_handler', 'list_bundle_suggestions_handler', 'list_runs_handler', 'get_latest_run_handler', 'list_removal_scenarios_handler', 'get_bundle_suggestion_handler', 'get_latest_item_kpi_handler', 'get_removal_scenario_handler', 'create_run_handler', 'get_price_suggestion_handler', 'record_decision_handler', 'list_decisions_handler', 'list_price_suggestions_handler', 'get_calibration_handler', 'set_bundle_promoted_handler', 'get_active_run_handler'}

missing = actix_routes - openapi_routes - menu_advisor
for r in sorted(missing):
    print(r)
