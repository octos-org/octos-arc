const http = require('http');
const fs = require('fs');
const path = require('path');
const url = require('url');
const crypto = require('crypto');
const qs = require('querystring');

const SALT = 'railway-demo-salt-2024';
const DATA_DIR = path.join(__dirname, 'data');
const ACCOUNTS_FILE = path.join(DATA_DIR, 'accounts.json');

let accounts = loadAccounts();
const sessions = {}; // token -> username

function loadAccounts() {
  try {
    if (fs.existsSync(ACCOUNTS_FILE)) {
      const parsed = JSON.parse(fs.readFileSync(ACCOUNTS_FILE, 'utf8'));
      return Array.isArray(parsed) ? parsed : [];
    }
  } catch (e) {}
  return [];
}
function saveAccounts() {
  try {
    if (!fs.existsSync(DATA_DIR)) fs.mkdirSync(DATA_DIR, { recursive: true });
    const tmp = ACCOUNTS_FILE + '.tmp';
    fs.writeFileSync(tmp, JSON.stringify(accounts));
    fs.renameSync(tmp, ACCOUNTS_FILE);
  } catch (e) {}
}
function hashPassword(pw) {
  return crypto.scryptSync(pw, SALT, 64, { N: 4096, r: 8, p: 1 }).toString('hex');
}
function createSession(username) {
  const token = crypto.randomBytes(16).toString('hex');
  sessions[token] = username;
  return token;
}
function sessionUser(req) {
  const cookie = req.headers.cookie || '';
  const m = cookie.match(/(?:^|;\s*)sid=([^;]+)/);
  if (!m) return null;
  return sessions[m[1]] || null;
}
function setSessionCookie(res, token) {
  res.setHeader('Set-Cookie', 'sid=' + token + '; HttpOnly; Path=/; SameSite=Lax; Max-Age=604800');
}
function clearSessionCookie(res) {
  res.setHeader('Set-Cookie', 'sid=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0');
}
function send(res, code, html) {
  res.writeHead(code, { 'Content-Type': 'text/html; charset=utf-8' });
  res.end(html);
}
function redirect(res, location) {
  res.writeHead(302, { 'Location': location });
  res.end();
}
function escapeHtml(s) {
  return String(s == null ? '' : s).replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
  }[c]));
}
function parseBody(req, body) {
  const ctype = req.headers['content-type'] || '';
  if (ctype.indexOf('application/json') !== -1) {
    try { return JSON.parse(body || '{}'); } catch (e) { return {}; }
  }
  return qs.parse(body || '');
}

function renderIndex(username) {
  const header = username
    ? '<div id="user-info"><span id="username">' + escapeHtml(username) + '</span><a href="/logout" id="sign-out">Sign out</a></div>'
    : '<div id="auth-links"><a href="/register">Register</a><a href="/login">Login</a></div>';
  return '<!DOCTYPE html><html lang="zh"><head><meta charset="UTF-8"><title>首页</title></head><body>' +
    header + '<h1>火车票预订</h1></body></html>';
}

let registerTemplate = '';
let loginTemplate = '';
function loadTemplates() {
  const regCandidates = [
    path.join(__dirname, '..', 'frontend', 'src', 'register.html'),
    path.join(__dirname, '..', 'frontend', 'dist', 'register.html')
  ];
  for (const c of regCandidates) {
    try { if (fs.existsSync(c)) { registerTemplate = fs.readFileSync(c, 'utf8'); break; } } catch (e) {}
  }
  const loginCandidates = [
    path.join(__dirname, '..', 'frontend', 'src', 'login.html'),
    path.join(__dirname, '..', 'frontend', 'dist', 'login.html')
  ];
  for (const c of loginCandidates) {
    try { if (fs.existsSync(c)) { loginTemplate = fs.readFileSync(c, 'utf8'); break; } } catch (e) {}
  }
  if (!registerTemplate) registerTemplate = '<!DOCTYPE html><html><body><form method="POST" action="/api/register" novalidate><div role="alert" id="alert">__ERROR__</div><button>下一步</button></form></body></html>';
  if (!loginTemplate) loginTemplate = '<!DOCTYPE html><html><body><form method="POST" action="/api/login" novalidate><div role="alert" id="alert">__ERROR__</div><button>登录</button></form></body></html>';
}
function renderRegister(error) {
  return registerTemplate.replace('__ERROR__', error || '');
}
function renderLogin(error) {
  return loginTemplate.replace('__ERROR__', error || '');
}

function validateRegister(data, errors) {
  const username = String(data.username == null ? '' : data.username).trim();
  if (!/^[A-Za-z0-9_-]{3,32}$/.test(username)) {
    errors.push('用户名格式不正确');
  }
  const password = String(data.password == null ? '' : data.password);
  if (!/^(?=.*[a-z])(?=.*[A-Z])(?=.*\d)(?=.*[^A-Za-z0-9]).{12,128}$/.test(password)) {
    errors.push('登录密码需为12-128位并包含大写字母、小写字母、数字和特殊字符');
  }
  if (password !== String(data.confirmPassword == null ? '' : data.confirmPassword)) {
    errors.push('确认密码必须与密码一致');
  }
  const docTypes = ['passport', 'id_card', 'hk_macau', 'tw_mainland', 'foreigner'];
  if (!docTypes.includes(data.documentType)) {
    errors.push('请选择有效的证件类型');
  }
  const name = String(data.name == null ? '' : data.name).trim();
  if (name.length < 2 || name.length > 100) {
    errors.push('姓名必须为2-100个字符');
  }
  if (!/^[A-Za-z0-9-]{6,30}$/.test(String(data.documentNumber == null ? '' : data.documentNumber))) {
    errors.push('证件号码不能为空');
  }
  const discountTypes = ['adult', 'child', 'student', 'disabled', 'other'];
  if (!discountTypes.includes(data.discountType)) {
    errors.push('请选择有效的优惠（待）类型');
  }
  if (!/^\+[0-9]{1,4}$/.test(String(data.countryCode == null ? '' : data.countryCode))) {
    errors.push('国家/地区代码无效');
  }
  if (!/^[0-9]{5,15}$/.test(String(data.mobileNumber == null ? '' : data.mobileNumber))) {
    errors.push('手机号不能为空');
  }
  if (data.terms !== 'on') {
    errors.push('请先阅读并同意服务条款和隐私政策');
  }
  const email = String(data.email == null ? '' : data.email).trim();
  if (email !== '') {
    if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email) || email.length > 254) {
      errors.push('邮箱格式不正确');
    }
  }
  if (accounts.some(a => a.username === username)) {
    errors.push('用户名已存在');
  }
  if (email && accounts.some(a => a.email && a.email.toLowerCase() === email.toLowerCase())) {
    errors.push('邮箱已存在');
  }
}

function handleRegister(req, res) {
  let body = '';
  req.on('data', c => { body += c; });
  req.on('end', () => {
    const data = parseBody(req, body);
    const errors = [];
    validateRegister(data, errors);
    if (errors.length > 0) {
      return send(res, 400, renderRegister(errors[0]));
    }
    const account = {
      username: String(data.username).trim(),
      password: hashPassword(String(data.password)),
      email: String(data.email == null ? '' : data.email).trim(),
      name: String(data.name).trim(),
      documentType: data.documentType,
      documentNumber: String(data.documentNumber),
      discountType: data.discountType,
      countryCode: String(data.countryCode),
      mobileNumber: String(data.mobileNumber)
    };
    accounts.push(account);
    saveAccounts();
    const token = createSession(account.username);
    setSessionCookie(res, token);
    return redirect(res, '/');
  });
}

function handleLogin(req, res) {
  let body = '';
  req.on('data', c => { body += c; });
  req.on('end', () => {
    const data = parseBody(req, body);
    const ident = String(data.usernameOrEmail == null ? '' : data.usernameOrEmail).trim();
    const password = String(data.password == null ? '' : data.password);
    const acct = accounts.find(a =>
      a.username === ident || (a.email && a.email.toLowerCase() === ident.toLowerCase())
    );
    if (!acct || acct.password !== hashPassword(password)) {
      return send(res, 401, renderLogin('账号或密码错误'));
    }
    const token = createSession(acct.username);
    setSessionCookie(res, token);
    return redirect(res, '/');
  });
}

function handleLogout(req, res) {
  const cookie = req.headers.cookie || '';
  const m = cookie.match(/(?:^|;\s*)sid=([^;]+)/);
  if (m) delete sessions[m[1]];
  clearSessionCookie(res);
  return redirect(res, '/');
}

function handler(req, res) {
  try {
    const parsedUrl = url.parse(req.url, true);
    const pathname = parsedUrl.pathname;
    if (pathname === '/api/register' && req.method === 'POST') { handleRegister(req, res); return; }
    if (pathname === '/api/login' && req.method === 'POST') { handleLogin(req, res); return; }
    if (pathname === '/logout') { handleLogout(req, res); return; }
    if (pathname === '/') { send(res, 200, renderIndex(sessionUser(req))); return; }
    if (pathname === '/register') { send(res, 200, renderRegister('')); return; }
    if (pathname === '/login') { send(res, 200, renderLogin('')); return; }
    let rel = pathname;
    if (!path.extname(rel)) rel += '.html';
    const filePath = path.join(__dirname, '..', 'frontend', 'dist', rel);
    fs.readFile(filePath, (err, data) => {
      if (err) {
        res.writeHead(404, { 'Content-Type': 'text/plain' });
        res.end('Not Found');
        return;
      }
      res.writeHead(200, { 'Content-Type': 'text/html' });
      res.end(data);
    });
  } catch (e) {
    res.writeHead(500, { 'Content-Type': 'text/plain' });
    res.end('Server Error');
  }
}

loadTemplates();
process.on('uncaughtException', err => {
  console.error(err);
});

const primary = http.createServer(handler);
primary.listen(process.env.PORT || 3000, () => {
  console.log('Primary server on ' + (process.env.PORT || 3000));
});

if (process.env.ARC_EXTRA_PORTS !== '0') {
  const extra = http.createServer(handler);
  extra.listen(3301, () => {
    console.log('Extra server on 3301');
  });
}
