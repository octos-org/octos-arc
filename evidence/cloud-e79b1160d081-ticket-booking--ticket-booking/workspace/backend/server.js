const http = require('http');
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const { URL } = require('url');

const PORT = process.env.PORT || 3000;
const EXTRA_PORT = 3301;
const distDir = path.join(__dirname, '..', 'frontend', 'dist');
const dataDir = path.join(__dirname, 'data');
const accountsFile = path.join(dataDir, 'accounts.json');

if (!fs.existsSync(dataDir)) {
  fs.mkdirSync(dataDir, { recursive: true });
}
if (!fs.existsSync(accountsFile)) {
  fs.writeFileSync(accountsFile, '[]');
}

let accounts = [];
try {
  accounts = JSON.parse(fs.readFileSync(accountsFile, 'utf8'));
} catch (e) {
  accounts = [];
}

const sessions = new Map();

function writeAccounts() {
  const tmp = accountsFile + '.tmp';
  fs.writeFileSync(tmp, JSON.stringify(accounts, null, 2));
  fs.renameSync(tmp, accountsFile);
}

function createSession(username) {
  const token = crypto.randomBytes(32).toString('hex');
  sessions.set(token, username);
  return token;
}

function destroySession(token) {
  sessions.delete(token);
}

function getUsernameFromCookie(header) {
  if (!header) return null;
  const cookies = header.split(';').map(c => c.trim());
  for (const cookie of cookies) {
    const [name, value] = cookie.split('=');
    if (name === 'session_token' && sessions.has(value)) {
      return sessions.get(value);
    }
  }
  return null;
}

function sendJson(res, status, data) {
  res.writeHead(status, { 'Content-Type': 'application/json' });
  res.end(JSON.stringify(data));
}

function validateRegistration(data) {
  const { username, password, confirmPassword, documentType, name, documentNumber, discountType, email, countryCode, mobile, terms } = data;
  if (!username || typeof username !== 'string') return '用户名必填';
  if (!password || typeof password !== 'string') return '登录密码必填';
  if (!confirmPassword || typeof confirmPassword !== 'string') return '确认密码必填';
  if (!documentType || typeof documentType !== 'string') return '证件类型必填';
  if (!name || typeof name !== 'string') return '姓名必填';
  if (!documentNumber || typeof documentNumber !== 'string') return '证件号码必填';
  if (!discountType || typeof discountType !== 'string') return '优惠（待）类型必填';
  if (!countryCode || typeof countryCode !== 'string') return '国家/地区代码必填';
  if (!mobile || typeof mobile !== 'string') return '手机号必填';
  if (!terms) return '请同意服务条款和隐私政策';
  if (!/^[a-zA-Z0-9_-]{3,32}$/.test(username)) return '用户名格式不正确';
  const trimmedName = name.trim();
  const nonSpaceCount = trimmedName.replace(/\s/g, '').length;
  if (nonSpaceCount < 2 || nonSpaceCount > 100) return '姓名格式不正确';
  if (!/^[a-zA-Z0-9-]{6,30}$/.test(documentNumber)) return '证件号码格式不正确';
  const validDocTypes = ['resident_id', 'passport', 'hk_macau', 'taiwan', 'foreign_permanent'];
  if (!validDocTypes.includes(documentType)) return '证件类型无效';
  const validDiscountTypes = ['adult', 'child', 'student', 'disabled_soldier', 'other'];
  if (!validDiscountTypes.includes(discountType)) return '优惠（待）类型无效';
  if (!/^\d{5,15}$/.test(mobile)) return '手机号格式不正确';
  if (!/^\+\d{1,3}$/.test(countryCode)) return '国家/地区代码格式不正确';
  if (email) {
    if (email.length > 254 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) return '邮箱格式不正确';
  }
  if (password.length < 12 || password.length > 128 ||
      !/(?=.*[a-z])(?=.*[A-Z])(?=.*\d)(?=.*[^A-Za-z0-9])/.test(password)) {
    return '登录密码需至少12位且包含大写字母、小写字母、数字和特殊字符';
  }
  if (password !== confirmPassword) return '确认密码必须一致';
  if (accounts.some(a => a.username === username)) return '用户名已存在';
  if (accounts.some(a => a.email.toLowerCase() === (email || '').toLowerCase())) return '邮箱已存在';
  return null;
}

function handleApiRegister(req, res, body) {
  try {
    const data = JSON.parse(body);
    const validationError = validateRegistration(data);
    if (validationError) {
      return sendJson(res, 400, { error: validationError });
    }
    const salt = crypto.randomBytes(16).toString('hex');
    const passwordHash = crypto.scryptSync(data.password, salt, 64, { N: 4096, r: 8, p: 1 }).toString('hex');
    const account = {
      id: crypto.randomUUID(),
      username: data.username,
      email: data.email || '',
      name: data.name.trim(),
      documentType: data.documentType,
      documentNumber: data.documentNumber,
      discountType: data.discountType,
      countryCode: data.countryCode,
      mobileNumber: data.mobile,
      passwordHash,
      salt
    };
    accounts.push(account);
    writeAccounts();
    const token = createSession(data.username);
    res.setHeader('Set-Cookie', `session_token=${token}; HttpOnly; Path=/; Max-Age=604800; SameSite=Lax`);
    return sendJson(res, 200, { username: data.username });
  } catch (e) {
    console.error('Register error:', e);
    return sendJson(res, 500, { error: '服务器错误' });
  }
}

function handleApiLogin(req, res, body) {
  try {
    const data = JSON.parse(body);
    const input = (data.usernameOrEmail || '').trim();
    const password = data.password || '';
    if (!input || !password) {
      return sendJson(res, 401, { error: '无效凭据' });
    }
    const isEmail = input.includes('@');
    const account = isEmail
      ? accounts.find(a => a.email.toLowerCase() === input.toLowerCase())
      : accounts.find(a => a.username === input);
    if (!account) {
      return sendJson(res, 401, { error: '无效凭据' });
    }
    const hash = crypto.scryptSync(password, account.salt, 64, { N: 4096, r: 8, p: 1 }).toString('hex');
    if (hash !== account.passwordHash) {
      return sendJson(res, 401, { error: '无效凭据' });
    }
    const token = createSession(account.username);
    res.setHeader('Set-Cookie', `session_token=${token}; HttpOnly; Path=/; Max-Age=604800; SameSite=Lax`);
    return sendJson(res, 200, { username: account.username });
  } catch (e) {
    console.error('Login error:', e);
    return sendJson(res, 500, { error: '服务器错误' });
  }
}

function handleApiSession(req, res) {
  const username = getUsernameFromCookie(req.headers.cookie);
  if (username) {
    return sendJson(res, 200, { username });
  } else {
    return sendJson(res, 401, { error: '未登录' });
  }
}

function serveIndexPage(res, username) {
  const userHtml = username
    ? `<span id="username">${username}</span><a href="/logout">退出登录</a>`
    : `<a href="/login">登录</a><a href="/register">Register</a>`;
  const html = `<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<title>Home</title>
</head>
<body>
<div id="guest"${username ? ' style="display:none;"' : ''}>${username ? '' : '<a href="/login">登录</a><a href="/register">Register</a>'}</div>
<div id="user"${username ? '' : ' style="display:none;"'}>${username ? '<span id="username">' + username + '</span><a href="/logout">退出登录</a>' : ''}</div>
</body>
</html>`;
  res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
  res.end(html);
}

function handler(req, res) {
  const reqUrl = new URL(req.url, 'http://localhost');
  const pathname = reqUrl.pathname;

  if (pathname === '/') {
    const username = getUsernameFromCookie(req.headers.cookie);
    serveIndexPage(res, username);
    return;
  }
  if (pathname === '/register') {
    fs.createReadStream(path.join(distDir, 'register.html')).pipe(res);
    return;
  }
  if (pathname === '/login') {
    fs.createReadStream(path.join(distDir, 'login.html')).pipe(res);
    return;
  }

  if (pathname === '/api/register' && req.method === 'POST') {
    let body = '';
    req.on('data', chunk => body += chunk);
    req.on('end', () => handleApiRegister(req, res, body));
    return;
  }

  if (pathname === '/api/login' && req.method === 'POST') {
    let body = '';
    req.on('data', chunk => body += chunk);
    req.on('end', () => handleApiLogin(req, res, body));
    return;
  }

  if (pathname === '/api/session' && req.method === 'GET') {
    handleApiSession(req, res);
    return;
  }

  if (pathname === '/logout') {
    const token = req.headers.cookie?.split(';').map(c => c.trim())
      .find(c => c.startsWith('session_token='))?.split('=')[1];
    if (token) destroySession(token);
    res.writeHead(302, { Location: '/' });
    res.end();
    return;
  }

  res.writeHead(404, { 'Content-Type': 'text/plain' });
  res.end('Not Found');
}

const server = http.createServer(handler);
server.listen(PORT, () => console.log(`Server running on port ${PORT}`));

if (process.env.ARC_EXTRA_PORTS !== '0') {
  const extraServer = http.createServer(handler);
  extraServer.listen(EXTRA_PORT, () => console.log(`Server also running on port ${EXTRA_PORT}`));
}

process.on('uncaughtException', (err) => {
  console.error('Uncaught exception:', err);
});
process.on('unhandledRejection', (reason) => {
  console.error('Unhandled rejection:', reason);
});
