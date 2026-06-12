# Sufrix POS — Agent Implementation Prompt

**Working directory:** `/Users/shawket/Desktop/sufrix_pos`
**Backend:** Already complete — zero backend changes required.
**Goal:** Implement device setup flow (one-time manager login → branch selection) so teller PIN login can include the required `branch_id`, plus handle two new API behaviours.

---

## Stack context

- Flutter / Dart 3.2+, all platforms (iOS, Android, macOS, Windows)
- State management: Riverpod (`ref.watch`, `ref.read`, `@riverpod`)
- Navigation: Go Router (`lib/core/router/router.dart`)
- HTTP: Dio — `lib/core/api/client.dart`; base URL in `lib/core/config/api_config.dart`
- KV storage (SQLite-backed, in-memory cache facade): `lib/core/storage/storage_service.dart`
- Secure token storage: `lib/core/storage/secure_token_store.dart`
- Auth state: `lib/core/providers/auth_notifier.dart` + `lib/core/repositories/auth_repository.dart`
- Auth API: `lib/core/api/auth_api.dart`
- Branch API: `lib/core/api/branch_api.dart`
- Branch model: `lib/core/models/branch.dart` (fields: id, orgId, name, address, phone, printerBrand, printerIp, printerPort, isActive, orgLogoUrl)
- User model: `lib/core/models/user.dart` (fields: id, orgId, branchId, name, email, role, isActive)
- Login screen: `lib/features/auth/login_screen.dart` (name text field + PIN pad, auto-submits at 4 digits)
- Settings screen: `lib/features/settings/settings_screen.dart`
- KV store: all reads are synchronous from in-memory cache (`_kv.get`); all writes are async (`await _kv.set`)

---

## Why this is needed

The backend PIN login (`POST /auth/login`) now **requires `branch_id`** in the request body:

```json
// Old (rejected with 400)
{ "name": "Ahmed", "pin": "1234" }

// New (required)
{ "name": "Ahmed", "pin": "1234", "branch_id": "<uuid>" }
```

The device has no concept of which org or branch it belongs to. A one-time "Device Setup" screen must collect this by having a manager log in with email + password, then pick a branch. After setup, all teller PIN logins automatically include the stored `branch_id`.

---

## Changes overview

| File | Type | Change |
|------|------|--------|
| `lib/core/storage/storage_service.dart` | edit | Add device config keys + 4 methods |
| `lib/core/api/auth_api.dart` | edit | Add `loginWithEmailPassword`; add `branchId` param to `loginWithPin` |
| `lib/core/api/branch_api.dart` | edit | Add `list()` method |
| `lib/core/repositories/auth_repository.dart` | edit | Add `setupDevice` + `confirmDeviceBranch`; fix `login` to send `branch_id` |
| `lib/core/providers/auth_notifier.dart` | edit | Add `fetchBranchesForSetup` + `confirmBranch` |
| `lib/core/router/router.dart` | edit | Add `/device-setup` route + updated redirect logic |
| `lib/features/settings/settings_screen.dart` | edit | Add "Reconfigure Device" tile |
| `lib/features/auth/login_screen.dart` | edit | Show stored branch name below logo |
| `lib/features/setup/device_setup_screen.dart` | **new** | Two-step wizard: manager login → branch picker |

---

## 1. Storage — `lib/core/storage/storage_service.dart`

Add three KV keys and four methods alongside the existing ones. Follow the exact same pattern used for existing keys (`_kv.set`, `_kv.get`, `_kv.remove`):

```dart
static const _kDeviceOrgId     = 'device_org_id';
static const _kDeviceBranchId  = 'device_branch_id';
static const _kDeviceBranchName = 'device_branch_name';

Future<void> saveDeviceConfig({
  required String orgId,
  required String branchId,
  required String branchName,
}) async {
  await _kv.set(_kDeviceOrgId,      orgId);
  await _kv.set(_kDeviceBranchId,   branchId);
  await _kv.set(_kDeviceBranchName, branchName);
}

String? get deviceOrgId      => _kv.get(_kDeviceOrgId);
String? get deviceBranchId   => _kv.get(_kDeviceBranchId);
String? get deviceBranchName => _kv.get(_kDeviceBranchName);

bool get isDeviceConfigured =>
    deviceOrgId != null && deviceBranchId != null;

Future<void> clearDeviceConfig() async {
  await _kv.remove(_kDeviceOrgId);
  await _kv.remove(_kDeviceBranchId);
  await _kv.remove(_kDeviceBranchName);
}
```

---

## 2. Auth API — `lib/core/api/auth_api.dart`

### 2a. Add email/password login method

```dart
Future<Map<String, dynamic>> loginWithEmailPassword({
  required String email,
  required String password,
}) async {
  final res = await _c.dio.post('/auth/login', data: {
    'email':    email,
    'password': password,
  });
  return res.data as Map<String, dynamic>;
}
```

### 2b. Update existing `loginWithPin` to include `branchId`

```dart
Future<Map<String, dynamic>> loginWithPin({
  required String name,
  required String pin,
  required String branchId,   // ADD THIS PARAMETER
}) async {
  final res = await _c.dio.post('/auth/login', data: {
    'name':      name,
    'pin':       pin,
    'branch_id': branchId,    // ADD THIS FIELD
  });
  return res.data as Map<String, dynamic>;
}
```

---

## 3. Branch API — `lib/core/api/branch_api.dart`

Add a `list()` method. This is called after the manager's token is already set in the Dio client headers (via `setAuthToken`), so no explicit auth parameter is needed. Filter to active branches only:

```dart
Future<List<Branch>> list() async {
  final res = await _c.dio.get('/branches');
  return (res.data as List<dynamic>)
      .map((j) => Branch.fromJson(j as Map<String, dynamic>))
      .where((b) => b.isActive)
      .toList();
}
```

---

## 4. Auth Repository — `lib/core/repositories/auth_repository.dart`

### 4a. Add `setupDevice()` — step 1

Logs in with manager credentials, temporarily sets the token to fetch branches, then discards the manager token. The device uses only teller PIN sessions during normal operation.

```dart
/// Step 1 of device setup: manager logs in, fetches branch list,
/// then clears the manager token — device uses teller PIN sessions only.
Future<List<Branch>> setupDevice({
  required String email,
  required String password,
}) async {
  final data = await _api.loginWithEmailPassword(
    email: email,
    password: password,
  );
  final token = data['token'] as String;
  setAuthToken(token);
  try {
    return await ref.read(branchApiProvider).list();
  } finally {
    // Always discard manager token regardless of success or failure
    setAuthToken('');
    await _storage.saveToken('');
  }
}
```

### 4b. Add `confirmDeviceBranch()` — step 2

```dart
/// Step 2 of device setup: persist the chosen branch as device config.
Future<void> confirmDeviceBranch(Branch branch) async {
  await _storage.saveDeviceConfig(
    orgId:      branch.orgId,
    branchId:   branch.id,
    branchName: branch.name,
  );
}
```

### 4c. Fix existing `login()` to include `branch_id`

Find the existing `_api.loginWithPin(...)` call and update it to read `deviceBranchId` from storage:

```dart
// Inside the existing login() method:
final branchId = _storage.deviceBranchId;
if (branchId == null) throw Exception('Device not configured — complete setup first');

final data = await _api.loginWithPin(
  name:     name,
  pin:      pin,
  branchId: branchId,   // ADD THIS
);
```

---

## 5. Auth Notifier — `lib/core/providers/auth_notifier.dart`

Add two methods. Use the existing `friendlyError()` helper for error formatting:

```dart
/// Step 1 of setup: manager credentials → branch list.
Future<({String? error, List<Branch> branches})> fetchBranchesForSetup({
  required String email,
  required String password,
}) async {
  try {
    final branches = await ref.read(authRepositoryProvider)
        .setupDevice(email: email, password: password);
    return (error: null, branches: branches);
  } catch (e) {
    return (error: friendlyError(e), branches: <Branch>[]);
  }
}

/// Step 2 of setup: manager has chosen a branch — persist device config.
Future<void> confirmBranch(Branch branch) async {
  await ref.read(authRepositoryProvider).confirmDeviceBranch(branch);
  // Force the router redirect to re-evaluate (device is now configured)
  ref.invalidateSelf();
}
```

---

## 6. Router — `lib/core/router/router.dart`

### 6a. Add the new route

Add as a top-level `GoRoute` (NOT inside the navigation shell):

```dart
GoRoute(
  path: '/device-setup',
  builder: (_, __) => const DeviceSetupScreen(),
),
```

### 6b. Replace the existing redirect closure

```dart
redirect: (context, state) {
  final auth       = ref.read(authProvider);
  final loading    = auth.isLoading;
  final authed     = auth.isAuthenticated;
  final storage    = ref.read(storageServiceProvider);
  final configured = storage.isDeviceConfigured;
  final loc        = state.matchedLocation;

  if (loading) return null;

  // Not configured → device setup must happen before anything else
  if (!configured && loc != '/device-setup') return '/device-setup';

  // Configured but not logged in → teller PIN login
  if (configured && !authed && loc != '/login' && loc != '/device-setup') {
    return '/login';
  }

  // Logged in but still on login or setup screen → go home
  if (authed && (loc == '/login' || loc == '/device-setup')) return '/home';

  return null;
},
```

---

## 7. New screen — `lib/features/setup/device_setup_screen.dart`

Two-step wizard. Full-screen, no navigation chrome. Use a local `StatefulWidget` — no Riverpod state beyond calling notifier methods. Style to match `login_screen.dart` (same dark background, same logo treatment, same fonts/spacing).

**Step 0 — Manager login:**
- Title: "Connect Device"
- Subtitle: "Sign in with your manager account to link this device to your branch."
- Email `TextFormField` (keyboard type: email)
- Password `TextFormField` (obscured, eye-icon toggle)
- "Continue" button → calls `ref.read(authProvider.notifier).fetchBranchesForSetup(...)`
- Show `CircularProgressIndicator` while loading (disable button)
- On error: show red inline text below the button
- On success (branches returned): advance to Step 1

**Step 1 — Branch selection:**
- Back arrow returns to Step 0
- Title: "Select Branch"
- Subtitle: "Choose the branch this device will serve."
- `ListView.builder` of `ListTile`s — `branch.name` as title, `branch.address ?? ''` as subtitle
- On tile tap: `await ref.read(authProvider.notifier).confirmBranch(branch)` — then do nothing (router redirect handles navigation automatically)
- If list is empty: show "No active branches found." text with a retry/back option

**Suggested structure:**

```dart
class DeviceSetupScreen extends StatefulWidget {
  const DeviceSetupScreen({super.key});
  @override
  State<DeviceSetupScreen> createState() => _DeviceSetupScreenState();
}

class _DeviceSetupScreenState extends State<DeviceSetupScreen> {
  int _step = 0;
  bool _loading = false;
  String? _error;
  List<Branch> _branches = [];

  final _emailCtrl    = TextEditingController();
  final _passwordCtrl = TextEditingController();
  bool _obscurePass   = true;

  @override
  void dispose() {
    _emailCtrl.dispose();
    _passwordCtrl.dispose();
    super.dispose();
  }

  Future<void> _continue() async { ... }
  Future<void> _selectBranch(Branch branch) async { ... }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      body: SafeArea(
        child: _step == 0 ? _buildLoginStep() : _buildBranchStep(),
      ),
    );
  }
}
```

---

## 8. Settings screen — `lib/features/settings/settings_screen.dart`

Add a "Device" section above the Sign Out button. Read device branch name from `storageServiceProvider`. Block reconfiguration if a shift is currently open:

```dart
// New section — insert before the sign-out button
_SectionHeader(t('settings.device')),
ListTile(
  leading: const Icon(Icons.settings_input_component_outlined),
  title: Text(t('settings.reconfigure_device')),
  subtitle: Text(ref.watch(storageServiceProvider).deviceBranchName ?? ''),
  onTap: () async {
    // Block if shift is open
    if (ref.read(shiftProvider).hasOpenShift) {
      ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(t('settings.reconfigure_shift_open')),
      ));
      return;
    }
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (_) => AlertDialog(
        title: Text(t('settings.reconfigure_title')),
        content: Text(t('settings.reconfigure_body')),
        actions: [
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: Text(t('common.cancel')),
          ),
          TextButton(
            onPressed: () => Navigator.pop(context, true),
            child: Text(t('settings.reconfigure_confirm')),
          ),
        ],
      ),
    );
    if (confirmed == true) {
      await ref.read(storageServiceProvider).clearDeviceConfig();
      await ref.read(authProvider.notifier).signOut();
      // Router redirect will push to /device-setup automatically
    }
  },
),
```

Add i18n keys to both `en` and `ar` translation files:
- `settings.device` → "Device" / "الجهاز"
- `settings.reconfigure_device` → "Reconfigure Device" / "إعادة ضبط الجهاز"
- `settings.reconfigure_shift_open` → "Close the current shift before reconfiguring the device." / "أغلق الوردية الحالية قبل إعادة ضبط الجهاز."
- `settings.reconfigure_title` → "Reconfigure Device?" / "إعادة ضبط الجهاز؟"
- `settings.reconfigure_body` → "This will sign you out and require a manager login to reconnect." / "سيتم تسجيل خروجك وستحتاج إلى تسجيل دخول المدير لإعادة الاتصال."
- `settings.reconfigure_confirm` → "Reconfigure" / "إعادة الضبط"

---

## 9. Login screen — `lib/features/auth/login_screen.dart`

Show the configured branch name below the app logo so tellers know which branch this device is set to:

```dart
// Below the logo/title widget, add:
Consumer(
  builder: (context, ref, _) {
    final name = ref.watch(storageServiceProvider).deviceBranchName;
    if (name == null || name.isEmpty) return const SizedBox.shrink();
    return Padding(
      padding: const EdgeInsets.only(top: 4),
      child: Text(
        name,
        style: Theme.of(context)
            .textTheme
            .bodySmall
            ?.copyWith(color: Colors.white70),
      ),
    );
  },
),
```

---

## 10. Other backend changes to handle

### Teller void orders
`orders:update` is now granted to the teller role by default. Show the void/cancel button to tellers wherever it was previously hidden.

### Transfer deletion 409
`DELETE /inventory/transfers/{id}` now returns `409 Conflict` when deleting the transfer would send the destination branch's stock below zero (previously went negative silently). Handle this explicitly in the inventory transfer delete flow — show a user-friendly error message rather than a generic "something went wrong".

---

## Verification

**Fresh install / cleared storage:**
1. App opens → `/device-setup` screen appears (not the login screen)
2. Enter manager email + password → "Continue" → branch list loads
3. Tap a branch → redirected to `/login` (teller PIN screen)
4. Branch name shows below the logo on the login screen

**Normal teller PIN login:**
1. Enter name + correct PIN → 200 response, session starts
2. Confirm the network request body includes `branch_id`

**Wrong branch:**
1. Teller whose account is NOT assigned to this branch → 401 response
2. Shake animation + "Invalid PIN" error message (existing UX)

**Reconfigure:**
1. Settings → Reconfigure Device → confirm dialog → back to `/device-setup`
2. If shift is open, reconfiguration is blocked with a snackbar

**Offline (existing behaviour must be unchanged):**
1. Kill network, try to log in
2. Offline PIN unlock still works (it doesn't call the backend — no change needed)

**Flutter checks:**
```bash
flutter analyze        # 0 errors, 0 warnings
flutter test           # all existing tests pass
```
