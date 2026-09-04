#!/usr/bin/env python3
"""Generate the golden evaluation set from the LIVE registry.

Every expectation is read out of `src/analytics/{presets,schema}.rs` rather than
typed here, so a case can never expect a preset, dataset, measure or period that
does not exist. Re-run after changing the registry; the Rust test refuses cases
that reference anything unknown, so drift fails the build either way.

Phrasings are hand-written — that is the half a generator cannot produce, and
the half the eval exists to test. Arabic is Egyptian colloquial, not MSA
translations of the English.
"""
import json, pathlib, re, sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
PRESETS = set(
    re.findall(r'preset!\("([a-z_0-9]+)"', (ROOT / "src/analytics/presets.rs").read_text())
    + re.findall(r'^        id: "([a-z_0-9]+)",', (ROOT / "src/analytics/presets.rs").read_text(), re.M)
)
SCHEMA = (ROOT / "src/analytics/schema.rs").read_text()
DATASETS = re.findall(r'^        id: "([a-z_]+)",\n        title:', SCHEMA, re.M)
PERIODS = ["today","yesterday","this_week","last_week","this_month","last_month",
           "this_year","last_year","last_7_days","last_30_days","last_90_days",
           "last_12_months","all_time"]

NOW = "2026-09-03T10:00:00Z"   # Wednesday, Cairo. Frozen so periods are checkable.
cases = []

def add(cid, lang, q, expect, category, confidence="high", note=None):
    c = {"id": cid, "lang": lang, "question": q, "now": NOW,
         "category": category, "confidence": confidence, "expect": expect}
    if note: c["note"] = note
    cases.append(c)

def preset(p, period=None, **extra):
    e = {"tool": "run_preset", "preset": p}
    if period: e["period"] = period
    e.update(extra)
    return e

def query(dataset, dims=None, measures=None, period=None, **extra):
    e = {"tool": "query_metrics", "dataset": dataset}
    if dims: e["dimensions"] = dims
    if measures: e["measures"] = measures
    if period: e["period"] = period
    e.update(extra)
    return e

# ── 1. Preset routing, English + Arabic ─────────────────────────────────────
# (question_en, question_ar, preset, period)
ROUTING = [
 ("how much did we make today","عملنا كام النهاردة","revenue_total","today"),
 ("revenue yesterday","مبيعات امبارح","revenue_total","yesterday"),
 ("what did we take last month","كام عملنا الشهر اللي فات","revenue_total","last_month"),
 ("how many orders today","كام أوردر النهاردة","order_count_total","today"),
 ("average ticket this week","متوسط الفاتورة الأسبوع ده","avg_ticket","this_week"),
 ("give me a sales summary for last week","اديني ملخص مبيعات الأسبوع اللي فات","sales_summary","last_week"),
 ("revenue per day this month","المبيعات يوم بيوم الشهر ده","sales_by_day","this_month"),
 ("what hours are busiest","إمتى بنبقى أزحم","sales_by_hour","last_30_days"),
 ("which weekday sells most","أنهي يوم في الأسبوع بيبيع أكتر","sales_by_weekday","last_30_days"),
 ("compare my branches","قارنلي الفروع","sales_by_branch","last_30_days"),
 ("show me the busiest times grid","وريني جدول الزحمة","peak_hours_heatmap","last_30_days"),
 ("dine in versus delivery","الصالة ولا الدليفري","sales_by_order_type","last_30_days"),
 ("delivery revenue by channel","مبيعات الدليفري حسب القناة","sales_by_channel","last_30_days"),
 ("what sold best last month","إيه أكتر صنف اتباع الشهر اللي فات","top_products","last_month"),
 ("which items barely sell","أنهي أصناف مش بتتباع","worst_products","last_30_days"),
 ("revenue by category","المبيعات حسب الفئة","top_categories","last_30_days"),
 ("most profitable products","أكتر الأصناف ربح","product_profit","last_30_days"),
 ("which products have the thinnest margin","أنهي أصناف هامشها ضعيف","thin_margin_products","last_30_days"),
 ("best seller in each branch","أكتر صنف مبيعا في كل فرع","best_seller_per_branch","last_30_days"),
 ("how did people pay today","الناس دفعت إزاي النهاردة","payment_mix","today"),
 ("how much of our money is cash","كام من فلوسنا كاش","cash_vs_card","last_30_days"),
 ("who has drawer differences","مين عنده فروقات في الدرج","drawer_variance","last_30_days"),
 ("shift cash summary","ملخص كاش الورديات","shift_cash_summary","last_30_days"),
 ("how much did we give away in discounts","كام خصومات اداينا","discount_usage","last_30_days"),
 ("why are orders being voided","ليه الأوردرات بتتلغي","voids_by_reason","last_30_days"),
 ("which cashier voids the most","أنهي كاشير بيلغي أكتر","voids_by_cashier","last_30_days"),
 ("how are my waiters doing","الويترز عاملين إيه","waiter_performance","last_30_days"),
 ("cashier performance","أداء الكاشير","cashier_performance","last_30_days"),
 ("who is late the most","مين بيتأخر أكتر","lateness_by_employee","last_month"),
 ("overtime by employee","الأوفر تايم لكل موظف","overtime_by_employee","last_month"),
 ("attendance trend","الحضور على مدار الأيام","attendance_by_day","last_30_days"),
 ("what am I wasting money on","بخسر فلوس في إيه","waste_by_ingredient","last_30_days"),
 ("waste over time","الهالك على مدار الوقت","waste_trend","last_30_days"),
 ("what does the kitchen go through","المطبخ بيستهلك إيه","consumption_by_ingredient","last_30_days"),
 ("what stock goes missing","إيه اللي بيضيع من المخزن","shrinkage_by_ingredient","last_90_days"),
 ("why does stock go missing","ليه المخزون بيضيع","shrinkage_by_reason","last_90_days"),
 ("who do I spend the most with","بصرف على مين أكتر","spend_by_supplier","last_90_days"),
 ("what ingredients cost me most","أنهي مكونات بتكلفني أكتر","spend_by_ingredient","last_90_days"),
 ("how much did we get in tips","اخدنا كام بقشيش","tips_total","today"),
 ("tips over time","البقشيش على مدار الوقت","tips_by_day","last_30_days"),
 ("which waiter earns the most tips","أنهي ويتر بياخد بقشيش أكتر","tips_by_waiter","last_30_days"),
 ("how much of the tips is cash","كام من البقشيش كاش","cash_vs_card_tips","last_30_days"),
 ("show me refunds","وريني المرتجعات","refunds_by_day","last_30_days"),
]
# Questions with more than one correct route. Scoring these as misses would
# measure the eval's opinion, not the model's accuracy.
ALSO_OK = {
 "revenue_total": ["sales_summary"],          # "how much did we make" — a superset answers it
 "sales_by_hour": ["peak_hours_heatmap"],     # "busiest hours" — the grid says it too
 "peak_hours_heatmap": ["sales_by_hour"],
 "sales_summary": ["revenue_total"],
 "cash_vs_card": ["payment_mix"],             # cash share is visible in the mix
 "top_products": ["product_profit"],          # "best" by revenue or by profit
}
# Breakdowns a hand-composed query answers exactly as well as the preset.
CUSTOM_OK = {"sales_by_day","sales_by_weekday","sales_by_branch","sales_by_hour",
             "top_categories","tips_by_day","tips_by_waiter","attendance_by_day",
             "waste_trend","refunds_by_day","shift_cash_summary",
             "cash_vs_card_tips","consumption_by_ingredient"}

for i,(en,ar,p,per) in enumerate(ROUTING, 1):
    extra = {}
    if p in ALSO_OK: extra["accept_presets"] = ALSO_OK[p]
    if p in CUSTOM_OK: extra["accept_custom_query"] = True
    for lang,q in (("en",en),("ar",ar)):
        add(f"route-{lang}-{i:03d}",lang,q,preset(p,per,**extra),"tool_selection")

# ── 2. Period resolution ────────────────────────────────────────────────────
PERIOD_PHRASES = [
 ("en","revenue today","today"),("ar","مبيعات النهاردة","today"),
 ("en","revenue yesterday","yesterday"),("ar","مبيعات امبارح","yesterday"),
 ("en","revenue this week","this_week"),("ar","مبيعات الأسبوع ده","this_week"),
 ("en","revenue last week","last_week"),("ar","مبيعات الأسبوع اللي فات","last_week"),
 ("en","revenue this month","this_month"),("ar","مبيعات الشهر ده","this_month"),
 ("en","revenue last month","last_month"),("ar","مبيعات الشهر اللي فات","last_month"),
 ("en","revenue this year","this_year"),("ar","مبيعات السنة دي","this_year"),
 ("en","revenue last year","last_year"),("ar","مبيعات السنة اللي فاتت","last_year"),
 ("en","revenue over the last 7 days","last_7_days"),("ar","مبيعات آخر ٧ أيام","last_7_days"),
 ("en","revenue over the last 30 days","last_30_days"),("ar","مبيعات آخر ٣٠ يوم","last_30_days"),
 ("en","revenue over the last 90 days","last_90_days"),("ar","مبيعات آخر ٩٠ يوم","last_90_days"),
 ("en","revenue over the last 12 months","last_12_months"),("ar","مبيعات آخر ١٢ شهر","last_12_months"),
 ("en","revenue of all time","all_time"),("ar","المبيعات من الأول","all_time"),
]
for i,(lang,q,per) in enumerate(PERIOD_PHRASES,1):
    add(f"period-{i:03d}",lang,q,preset("revenue_total",per),"period_resolution")

# ── 3. Custom queries the presets do not cover ──────────────────────────────
CUSTOM = [
 ("en","tips per branch last month","orders",["branch"],["tip_total"],"last_month"),
 ("ar","البقشيش لكل فرع الشهر اللي فات","orders",["branch"],["tip_total"],"last_month"),
 ("en","units sold per category this week","order_items",["category"],["units_sold"],"this_week"),
 ("ar","الكميات المباعة لكل فئة الأسبوع ده","order_items",["category"],["units_sold"],"this_week"),
 ("en","refund value per branch","orders",["branch"],["refund_amount"],"last_30_days"),
 ("ar","قيمة المرتجعات لكل فرع","orders",["branch"],["refund_amount"],"last_30_days"),
 ("en","payment amounts by method per day","payments",["day"],["paid_amount"],"last_30_days"),
 ("ar","المدفوعات يوم بيوم","payments",["day"],["paid_amount"],"last_30_days"),
 ("en","waste cost per branch","inventory",["branch"],["movement_cost"],"last_30_days"),
 ("ar","تكلفة الهالك لكل فرع","inventory",["branch"],["movement_cost"],"last_30_days"),
 ("en","worked minutes per department","attendance",["department"],["worked_minutes"],"last_month"),
 ("ar","ساعات الشغل لكل قسم","attendance",["department"],["worked_minutes"],"last_month"),
 ("en","supplier fill rate","purchasing",["supplier"],["fill_rate"],"last_90_days"),
 ("ar","نسبة توريد كل مورد","purchasing",["supplier"],["fill_rate"],"last_90_days"),
 ("en","shrinkage by reason","stocktakes",["variance_reason"],["shrink_cost"],"last_90_days"),
 ("ar","الفاقد حسب السبب","stocktakes",["variance_reason"],["shrink_cost"],"last_90_days"),
 ("en","cash variance per teller","shifts",["teller"],["abs_discrepancy"],"last_30_days"),
 ("ar","فرق الكاش لكل كاشير","shifts",["teller"],["abs_discrepancy"],"last_30_days"),
 ("en","average tip per waiter this month","orders",["waiter"],["avg_tip"],"this_month"),
 ("ar","متوسط البقشيش لكل ويتر الشهر ده","orders",["waiter"],["avg_tip"],"this_month"),
 ("en","tip rate per branch","orders",["branch"],["tip_rate"],"last_30_days"),
 ("ar","نسبة البقشيش لكل فرع","orders",["branch"],["tip_rate"],"last_30_days"),
]
for i,(lang,q,ds,dims,meas,per) in enumerate(CUSTOM,1):
    add(f"custom-{i:03d}",lang,q,query(ds,dims,meas,per),"custom_query")

# ── 4. Entity kind distinction — the point of `analytics::entities` ─────────
ENTITY = [
 ("en","how did Ahmed do as a waiter last week","waiter","orders",["waiter"],"last_week"),
 ("ar","أحمد عمل إيه كويتر الأسبوع اللي فات","waiter","orders",["waiter"],"last_week"),
 ("en","how much did Ahmed ring up on the till","cashier","orders",["cashier"],"last_30_days"),
 ("ar","أحمد سحب كام على الكاشير","cashier","orders",["cashier"],"last_30_days"),
 ("en","was Ahmed late this month","employee","attendance",["employee"],"this_month"),
 ("ar","أحمد اتأخر الشهر ده","employee","attendance",["employee"],"this_month"),
 ("en","revenue at Marina","branch","orders",["branch"],"last_30_days"),
 ("ar","مبيعات فرع المارينا","branch","orders",["branch"],"last_30_days"),
 ("en","how many lattes did we sell","product","order_items",["product"],"last_30_days"),
 ("ar","بعنا كام لاتيه","product","order_items",["product"],"last_30_days"),
 ("en","how much milk did we go through","ingredient","inventory",["ingredient"],"last_30_days"),
 ("ar","استهلكنا كام لبن","ingredient","inventory",["ingredient"],"last_30_days"),
]
for i,(lang,q,kind,ds,dims,per) in enumerate(ENTITY,1):
    add(f"entity-{i:03d}",lang,q,query(ds,dims,None,per,entity_kind=kind),
        "entity_resolution","review",
        "A bare first name is ambiguous across kinds; the expected kind follows from the question's context word (waiter / till / late).")

# ── 5. Negative and error cases ─────────────────────────────────────────────
NEGATIVE = [
 ("en","revenue at Zamalek","unknown_entity","No branch by that name; the answer must say so rather than silently covering all branches."),
 ("ar","مبيعات فرع الزمالك","unknown_entity","نفس الحالة بالعربي."),
 ("en","what is the weather today","no_tool","Nothing in the registry answers this; it must decline rather than route somewhere plausible."),
 ("ar","الجو عامل إيه النهاردة","no_tool","نفس الحالة بالعربي."),
 ("en","revenue next month","invalid_period","A future window has no data; it must not silently answer for the past."),
 ("ar","مبيعات الشهر الجاي","invalid_period","نفس الحالة بالعربي."),
 ("en","what is my customers' phone list","refused","No measure returns customer contact data; there is nothing to query."),
 ("ar","اديني أرقام تليفونات العملاء","refused","نفس الحالة بالعربي."),
 ("en","show me profit margin on staff salaries","no_tool","Conflates two datasets that do not join; it should clarify rather than invent."),
 ("ar","وريني هامش الربح على مرتبات الموظفين","no_tool","نفس الحالة بالعربي."),
 ("en","how did it go","clarify","Too vague to route; asking is correct."),
 ("ar","عاملين إيه","clarify","نفس الحالة بالعربي."),
]
for i,(lang,q,outcome,why) in enumerate(NEGATIVE,1):
    add(f"negative-{i:03d}",lang,q,{"outcome":outcome},"negative","review",why)

# ── 6. Adversarial — designed to defeat the mechanisms, not exercise them ───
ADVERSARIAL = [
 ("ar","مبيعات ٢٠٢٦-٠٨-٠١","Arabic-Indic digits in a date must normalise to ASCII before parsing."),
 ("mixed","show me مبيعات last month","A sentence mixing scripts mid-way must still route."),
 ("en","and the one before that","A pronoun-only follow-up with no prior turn has nothing to resolve; it must ask."),
 ("en","how did Water do last week","A PRODUCT named like a common word, in a phrasing that reads as a person."),
 ("ar","الفرع الجديد عامل إيه","«the new branch» — most recently opened, or ask? A business decision, not a code fact."),
 ("ar","مبيعات الويك اند","«the weekend» — Fri–Sat, Sat–Sun or Fri–Sun for an Egyptian F&B tenant is a business decision."),
 ("en","best product","«best» by revenue, quantity or margin is genuinely ambiguous."),
 ("ar","أحسن صنف","نفس الغموض بالعربي."),
]
for i,(lang,q,why) in enumerate(ADVERSARIAL,1):
    add(f"adversarial-{i:03d}",lang,q,{"outcome":"review"},"adversarial","review",why)


# ── 7. Transforms: compare, share, cumulative, top-per ──────────────────────
TRANSFORMS = [
 ("en","compare this month to last month","orders",None,["revenue"],"this_month",{"compare":"previous_period"}),
 ("ar","قارن الشهر ده بالشهر اللي فات","orders",None,["revenue"],"this_month",{"compare":"previous_period"}),
 ("en","how does this month compare to the same month last year","orders",None,["revenue"],"this_month",{"compare":"previous_year"}),
 ("ar","الشهر ده مقارنة بنفس الشهر السنة اللي فاتت","orders",None,["revenue"],"this_month",{"compare":"previous_year"}),
 ("en","branch revenue versus last month","orders",["branch"],["revenue"],"this_month",{"compare":"previous_period"}),
 ("ar","مبيعات الفروع مقارنة بالشهر اللي فات","orders",["branch"],["revenue"],"this_month",{"compare":"previous_period"}),
 ("en","what share of revenue does each category take","order_items",["category"],["item_revenue"],"last_30_days",{"share":True}),
 ("ar","كل فئة بتاخد كام في المية من المبيعات","order_items",["category"],["item_revenue"],"last_30_days",{"share":True}),
 ("en","running total of revenue this month","orders",["day"],["revenue"],"this_month",{"cumulative":True}),
 ("ar","المبيعات التراكمية الشهر ده","orders",["day"],["revenue"],"this_month",{"cumulative":True}),
 ("en","top 3 products in each branch","order_items",["branch","product"],["item_revenue"],"last_30_days",{"top_per":"branch"}),
 ("ar","أعلى ٣ أصناف في كل فرع","order_items",["branch","product"],["item_revenue"],"last_30_days",{"top_per":"branch"}),
]
for i,(lang,q,ds,dims,meas,per,extra_args) in enumerate(TRANSFORMS,1):
    add(f"transform-{i:03d}",lang,q,query(ds,dims,meas,per,**extra_args),"transform")

# ── 8. Filters — the values that change what a number MEANS ─────────────────
FILTERS = [
 ("en","show me voided orders","orders",{"status":"voided"},"last_30_days"),
 ("ar","وريني الأوردرات الملغية","orders",{"status":"voided"},"last_30_days"),
 ("en","refunded orders this month","orders",{"status":"refunded"},"this_month"),
 ("ar","الأوردرات المرتجعة الشهر ده","orders",{"status":"refunded"},"this_month"),
 ("en","delivery orders only","orders",{"order_type":"delivery"},"last_30_days"),
 ("ar","أوردرات الدليفري بس","orders",{"order_type":"delivery"},"last_30_days"),
 ("en","dine in orders only","orders",{"order_type":"dine_in"},"last_30_days"),
 ("ar","أوردرات الصالة بس","orders",{"order_type":"dine_in"},"last_30_days"),
 ("en","orders that had a discount","orders",{"discounted":"yes"},"last_30_days"),
 ("ar","الأوردرات اللي عليها خصم","orders",{"discounted":"yes"},"last_30_days"),
 ("en","waste movements only","inventory",{"movement_type":"waste"},"last_30_days"),
 ("ar","حركات الهالك بس","inventory",{"movement_type":"waste"},"last_30_days"),
 ("en","stock we bought in","inventory",{"movement_type":"purchase"},"last_90_days"),
 ("ar","المخزون اللي اشتريناه","inventory",{"movement_type":"purchase"},"last_90_days"),
 ("en","who was absent last month","attendance",{"attendance_status":"absent"},"last_month"),
 ("ar","مين كان غايب الشهر اللي فات","attendance",{"attendance_status":"absent"},"last_month"),
]
for i,(lang,q,ds,filt,per) in enumerate(FILTERS,1):
    add(f"filter-{i:03d}",lang,q,query(ds,None,None,per,filters=filt),"filters")

# ── Regressions: questions that failed in production ────────────────────────
#
# Each of these is a real question a merchant asked that came back wrong or
# unanswered. They are kept as cases so the specific failure cannot return
# quietly; the note records what actually broke.

# The branch is "SIDI HENEISH"; merchants type it a dozen ways. Before the
# fuzzy matcher was fixed, anything but the exact spelling went unmatched and
# the whole question came back "I couldn't work that one out".
BRANCH_TYPOS = [
    ("en", "waiter performance in sidi henish this summer with tips"),
    ("en", "tips by waiter at sidi heneish over the summer"),
    ("en", "how did the waiters at Sidi Henish do with tips this summer"),
    ("ar", "أداء الويترز في سيدي حنيش الصيف ده بالتيبس"),
    ("en", "tips per waiter in heneish last 90 days"),
]
for i, (lang, q) in enumerate(BRANCH_TYPOS, 1):
    add(f"regress-branch-{i:03d}", lang, q,
        preset("tips_by_waiter", "last_90_days",
               accept_presets=["tips_total", "waiter_performance"],
               accept_custom_query=True),
        "regression", confidence="review",
        note="Real failure: the branch is spelled SIDI HENEISH and the merchant typed "
             "'sidi henish'. Substring matching missed it by one letter, so the branch went "
             "unmatched and no answer was produced. 'This summer' has no preset, so the "
             "period is a judgement call — a rolling 90 days is the closest available.")

# The summer window itself: there is no `this_summer` preset, and the model must
# pick a real one or an explicit range rather than inventing an id.
for i, (lang, q) in enumerate([
    ("en", "revenue across this summer"),
    ("ar", "مبيعات الصيف ده"),
    ("en", "how did we do over the summer months"),
], 1):
    add(f"regress-summer-{i:03d}", lang, q,
        preset("revenue_total", "last_90_days",
               accept_presets=["sales_summary", "sales_by_day"],
               accept_custom_query=True),
        "regression", confidence="review",
        note="'Summer' is not a period preset. The model must map it onto a real preset or an "
             "explicit from/to range; inventing 'this_summer' is the failure this pins.")

# Money reporting. The model once divided piastres by 1000 instead of 100, so
# the prose said 1,178.30 where the table beside it said 11,783. Amounts now
# reach the model already in pounds; these cases keep the routing honest.
for i, (lang, q) in enumerate([
    ("en", "total tips last month"),
    ("ar", "إجمالي التيبس الشهر اللي فات"),
    ("en", "which waiter earned the most in tips last month"),
    ("ar", "مين الويتر اللي خد أكتر تيبس الشهر اللي فات"),
], 1):
    tips_preset = "tips_total" if i <= 2 else "tips_by_waiter"
    add(f"regress-money-{i:03d}", lang, q,
        preset(tips_preset, "last_month",
               accept_presets=["tips_by_waiter", "tips_total"],
               accept_custom_query=True),
        "regression",
        note="Money is handed to the model already converted to pounds. This case exists "
             "because a stated figure once disagreed with the table beside it by 10x.")

# ── Validate everything against the real registry before writing ────────────
errors = []
for c in cases:
    e = c["expect"]
    if e.get("preset") and e["preset"] not in PRESETS:
        errors.append(f"{c['id']}: unknown preset {e['preset']}")
    # Alternatives are expectations too. An accept_presets entry naming a preset
    # that does not exist silently narrows the case to a single right answer.
    for alt in e.get("accept_presets") or []:
        if alt not in PRESETS:
            errors.append(f"{c['id']}: unknown accept_preset {alt}")
    if e.get("dataset") and e["dataset"] not in DATASETS:
        errors.append(f"{c['id']}: unknown dataset {e['dataset']}")
    if e.get("period") and e["period"] not in PERIODS:
        errors.append(f"{c['id']}: unknown period {e['period']}")
    for fk, fv in (e.get("filters") or {}).items():
        # Filter ids and their allowed values both come from the registry.
        if f'id: "{fk}"' not in SCHEMA:
            errors.append(f"{c['id']}: unknown filter '{fk}'")
        elif f'value: "{fv}"' not in SCHEMA:
            errors.append(f"{c['id']}: unknown value '{fv}' for filter '{fk}'")
if errors:
    print("\n".join(errors)); sys.exit(1)

out = ROOT / "evals/cases.json"
out.write_text(json.dumps({"now": NOW, "cases": cases}, ensure_ascii=False, indent=2) + "\n")

from collections import Counter
print(f"wrote {len(cases)} cases -> {out.relative_to(ROOT)}")
print(" by category:", dict(Counter(c["category"] for c in cases)))
print(" by language:", dict(Counter(c["lang"] for c in cases)))
print(" by confidence:", dict(Counter(c["confidence"] for c in cases)))
