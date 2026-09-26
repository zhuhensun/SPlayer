const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const { parse } = require('vue/compiler-sfc');
const { ref } = require('vue');

function setup(overrides = {}) {
  const source = fs.readFileSync(require.resolve('../src/views/Login/LoginView.vue'), 'utf8');
  const { descriptor } = parse(source);
  const script = descriptor.scriptSetup.content
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];/gm, '')
    .replace('import.meta.env.VITE_SITE_TITLE', '"Test Player"');
  const errors = [], successes = [], routes = [], calls = [];
  const user = { userLogin: false, setCookie(value) { this.cookie = value; }, setUserData(value) { this.profile = value; }, userLogOut() { this.userLogin = false; } };
  const context = {
    ref, userStore: () => user, musicStore: () => ({}), settingStore: () => ({}),
    useI18n: () => ({ t: (key) => key }), useRouter: () => ({ push: async (path) => routes.push(path) }),
    formRules: () => ({ mobileRule: { key: 'phone' } }),
    onMounted() {}, onBeforeUnmount() {}, setInterval: (fn) => fn, clearInterval() {},
    $message: { error: (value) => errors.push(value), success: (value) => successes.push(value) },
    $loadingBar: { error() {} }, $signIn() {},
    toLogin: async (...args) => { calls.push(args); return { code: 200, cookie: 'test-only-cookie' }; },
    getLoginState: async () => ({ data: { profile: { userId: 123 } } }),
    sentCaptcha: async () => ({ code: 200 }),
    ...overrides,
  };
  vm.createContext(context);
  vm.runInContext(script + '\nglobalThis.login = { phoneLogin, getCaptcha, phoneFormData, phoneFormRef, phoneLoginLoading, captchaSending, captchaDisabled, checkQrState, qrCheckInterval };', context);
  context.login.phoneFormRef.value = { validate: async () => {} };
  context.login.phoneFormData.value = { phone: 'test-phone', captcha: '012345' };
  return { ...context.login, errors, successes, routes, calls, user };
}
const event = { preventDefault() {} };

test('preserves leading-zero captcha and completes login once', async () => {
  const app = setup();
  await app.phoneLogin(event);
  assert.deepEqual(app.calls, [['test-phone', '012345']]);
  assert.equal(app.user.userLogin, true);
  assert.equal(app.successes.length, 1);
  assert.deepEqual(app.routes, ['/user']);
  assert.equal(app.phoneLoginLoading.value, false);
});
test('HTTP 400 surfaces upstream -460 without a false success', async () => {
  const app = setup({ toLogin: async () => { throw { response: { data: { code: -460, message: '检测到您的网络环境存在风险，请稍后再试' } } }; } });
  await app.phoneLogin(event);
  assert.match(app.errors[0], /网络环境.*-460/);
  assert.equal(app.successes.length, 0);
  assert.equal(app.routes.length, 0);
  assert.equal(app.phoneLoginLoading.value, false);
});
test('business errors in HTTP 200 are also displayed', async () => {
  const app = setup({ toLogin: async () => ({ code: 502, msg: '验证码错误' }) });
  await app.phoneLogin(event);
  assert.match(app.errors[0], /验证码错误.*502/);
  assert.equal(app.user.userLogin, false);
});
test('waits for session verification and suppresses duplicate submissions', async () => {
  let release;
  const app = setup({ getLoginState: () => new Promise((resolve) => { release = resolve; }) });
  const first = app.phoneLogin(event);
  await new Promise(setImmediate);
  await app.phoneLogin(event);
  assert.equal(app.calls.length, 1);
  assert.equal(app.successes.length, 0);
  assert.equal(app.routes.length, 0);
  release({ data: { profile: { userId: 123 } } });
  await first;
  assert.equal(app.routes.length, 1);
});
test('missing session does not save credentials or mark user logged in', async () => {
  const app = setup({ getLoginState: async () => ({ data: { profile: null } }) });
  await app.phoneLogin(event);
  assert.equal(app.user.cookie, undefined);
  assert.equal(app.user.userLogin, false);
  assert.equal(app.successes.length, 0);
  assert.match(app.errors[0], /Cookie/);
});
test('SMS send failure restores the button and displays upstream error', async () => {
  const app = setup({ sentCaptcha: async () => { throw { response: { data: { code: 405, message: '请稍后重试' } } }; } });
  await app.getCaptcha('test-phone');
  assert.equal(app.captchaSending.value, false);
  assert.equal(app.captchaDisabled.value, false);
  assert.match(app.errors[0], /405/);
});

test('QR login still completes when no intermediate 802 notification was seen', async () => {
  const app = setup({ checkQr: async () => ({ code: 803, cookie: 'test-only-qr-cookie' }) });
  app.checkQrState('test-qr-key');
  app.qrCheckInterval.value();
  await new Promise(setImmediate);
  assert.equal(app.user.userLogin, true);
  assert.deepEqual(app.routes, ['/user']);
  assert.equal(app.successes.length, 1);
  assert.equal(app.errors.length, 0);
});
test('SMS API credentials are sent in POST bodies, not URL queries', async () => {
  const source = fs.readFileSync(require.resolve('../src/api/login.js'), 'utf8')
    .replace(/^import[^;]+;/m, '').replace(/export const /g, 'const ');
  const requests = [];
  const context = { axios: (request) => requests.push(request) };
  vm.createContext(context);
  vm.runInContext(source + '\nglobalThis.api = { toLogin, sentCaptcha };', context);
  context.api.toLogin('test-phone', '012345');
  context.api.sentCaptcha('test-phone');
  for (const request of requests) {
    assert.equal(request.method, 'POST');
    assert.equal(request.params.phone, undefined);
    assert.equal(request.params.captcha, undefined);
    assert.equal(request.data.phone, 'test-phone');
  }
  assert.equal(requests[0].data.captcha, '012345');
});
