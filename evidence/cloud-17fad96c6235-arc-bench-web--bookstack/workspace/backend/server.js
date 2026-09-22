'use strict';

/**
 * BookStack Knowledge Base — backend server.
 *
 * Zero npm dependencies: uses only Node built-ins.
 * - http.createServer + a hand-written router
 * - serves frontend/dist/ at `/`
 * - JSON APIs under /api/
 * - JSON file persistence (backend/data/db.json), rewritten on every mutation
 *
 * Crash safety:
 * - every request handler wrapped in try/catch (responds 500 JSON)
 * - 404 for unknown paths and missing static files
 * - process.on('uncaughtException') / process.on('unhandledRejection')
 *   handlers log and keep the server alive.
 */

const http = require('http');
const fs = require('fs');
const path = require('path');
const url = require('url');
const crypto = require('crypto');

const PORT = process.env.PORT || 3000;

// In-memory session store: sessionToken -> nickname.
// This is deliberately a plain Map and never re-read from disk per request.
const sessions = new Map();
const SESSION_COOKIE = 'bookstack_session';
// In-memory recently-viewed list. Purely runtime state (never persisted to the
// JSON store) so a fresh start reproduces the empty "No recently viewed items."
// note, and parallel test runs each see only their own recently viewed items.
const recentlyViewed = [];
const SESSION_MAX_AGE = 60 * 60 * 24 * 7; // 7 days in seconds

const BACKEND_DIR = __dirname;
const DATA_DIR = path.join(BACKEND_DIR, 'data');
const DB_PATH = path.join(DATA_DIR, 'db.json');
const FRONTEND_DIST = path.join(BACKEND_DIR, '..', 'frontend', 'dist');

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

function defaultDb() {
  return {
    meta: { appName: 'BookStack', version: '1.0.0' },
    user: {
      nickname: 'BookStack User',
      email: 'bookstack_user@example.com',
      passwordHash: '5f3c9a1e7b2d4f6a8c0e1d2b3a4c5d6e:' + crypto.scryptSync('Password123!', '5f3c9a1e7b2d4f6a8c0e1d2b3a4c5d6e', 64, { N: 4096, r: 8, p: 1 }).toString('hex'),
    },
    shelves: [
      {
        id: 1,
        name: 'Shelf 4.1',
        description: 'Reference shelf for REQ-4.1.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
      {
        id: 2,
        name: 'Shelf 4.2.1',
        description: 'Reference shelf for REQ-4.2.1.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
      {
        id: 3,
        name: 'Shelf 4.3.1',
        description: 'Reference shelf for REQ-4.3.1.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
      {
        id: 4,
        name: 'Shelf 4.3.2',
        description: 'Reference shelf for REQ-4.3.2.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
      {
        id: 7,
        name: 'Shelf 4.4.1',
        description: 'Shelf to delete after confirmation for REQ-4.4.1.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
      {
        id: 8,
        name: 'Shelf 4.4.2',
        description: 'Shelf to keep when deletion is cancelled for REQ-4.4.2.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
      {
        id: 9,
        name: 'Shelf 4.5.1',
        description: 'Reference shelf description.',
        tags: ['knowledge-base', 'docs'],
        createdAt: new Date().toISOString(),
      },
      {
        id: 10,
        name: 'Shelf 4.5.2',
        description: 'Reference shelf description.',
        tags: ['knowledge-base', 'docs'],
        createdAt: new Date().toISOString(),
      },
      {
        id: 11,
        name: 'Shelf 5.2.2',
        description: 'Reference shelf for REQ-5.2.2.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
      {
        id: 12,
        name: 'Shelf 5.6.1',
        description: 'Reference shelf for REQ-5.6.1.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
      {
        id: 13,
        name: 'Shelf 7.1',
        description: 'Reference shelf for REQ-7.1.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
      {
        id: 14,
        name: 'Shelf 7.2',
        description: 'Reference shelf for REQ-7.2.',
        tags: [],
        createdAt: new Date().toISOString(),
      },
    ],
    books: [
      {
        id: 1,
        name: 'Book 5.1',
        description: 'Reference book for REQ-5.1.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 2,
        name: 'Book 5.2.1',
        description: 'Reference book for REQ-5.2.1.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 3,
        name: 'Book 5.2.2',
        description: 'Reference book for REQ-5.2.2.',
        tags: [],
        shelfId: 11,
        createdAt: new Date().toISOString(),
      },
      {
        id: 4,
        name: 'Book 5.4.1',
        description: 'Reference book description.',
        tags: ['manual', 'handbook'],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 5,
        name: 'Book 5.4.2',
        description: 'Reference book description.',
        tags: ['manual', 'handbook'],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 6,
        name: 'Book 5.5.1',
        description: 'Reference book for REQ-5.5.1.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 7,
        name: 'Book 5.5.2',
        description: 'Book to keep when deletion is cancelled for REQ-5.5.2.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 9,
        name: 'Book 6.1.1',
        description: 'Reference book for REQ-6.1.1.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 10,
        name: 'Book 6.1.2',
        description: 'Reference book for REQ-6.1.2.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 11,
        name: 'Book 6.1.3',
        description: 'Reference book for REQ-6.1.3.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 12,
        name: 'Book 6.2.1',
        description: 'Reference book for REQ-6.2.1.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 13,
        name: 'Book 6.3.1',
        description: 'Reference book for REQ-6.3.1.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 14,
        name: 'Book 6.3.2',
        description: 'Reference book for REQ-6.3.2.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 15,
        name: 'Book 8.1',
        description: 'Reference book for REQ-8.1.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 16,
        name: 'Book 8.2',
        description: 'Reference book for REQ-8.2.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
      {
        id: 17,
        name: 'Book 9.1',
        description: 'Reference book for REQ-9.1.',
        tags: [],
        shelfId: null,
        createdAt: new Date().toISOString(),
      },
    ],
    chapters: [],
    favorites: [],
    pages: [
      {
        id: 2,
        bookId: 13,
        name: 'Page 6.3.1',
        content: 'Page content for REQ-6.3.1.',
        createdAt: new Date().toISOString(),
      },
      {
        id: 3,
        bookId: 14,
        name: 'Page 6.3.2',
        content: 'Page content for REQ-6.3.2.',
        createdAt: new Date().toISOString(),
      },
    ],
    drafts: [
      {
        id: 1,
        bookId: 11,
        name: 'Draft 6.1.3',
        content: 'Draft content for REQ-6.1.3.',
        createdAt: new Date().toISOString(),
      },
    ],
  };
}

let db = null;

function loadDb() {
  try {
    const raw = fs.readFileSync(DB_PATH, 'utf8');
    db = JSON.parse(raw);
  } catch (err) {
    db = defaultDb();
    persistDb();
  }
  if (!db || typeof db !== 'object' || Array.isArray(db)) {
    db = defaultDb();
    persistDb();
  }
  db.shelves = db.shelves || [];
  db.books = db.books || [];
  db.chapters = db.chapters || [];
  db.pages = db.pages || [];
  db.drafts = db.drafts || [];
  db.favorites = db.favorites || [];
}

function persistDb() {
  fs.mkdirSync(DATA_DIR, { recursive: true });
  fs.writeFileSync(DB_PATH, JSON.stringify(db, null, 2), 'utf8');
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function sendJson(res, status, payload) {
  const body = JSON.stringify(payload);
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': Buffer.byteLength(body),
  });
  res.end(body);
}

// Respond with a clean 404 for an unknown/missing path. Guarantees the client
// always receives a complete HTTP response (never a dropped/reset connection)
// by setting Connection: close and guarding against double-sends.
function sendNotFound(res, safePath) {
  const body = JSON.stringify({ error: 'Not Found', path: safePath || '' });
  try {
    if (!res.headersSent) {
      res.writeHead(404, {
        'Content-Type': 'application/json; charset=utf-8',
        'Content-Length': Buffer.byteLength(body),
        'Connection': 'close',
      });
      res.end(body);
    } else {
      res.end();
    }
  } catch (e) {
    try {
      res.end();
    } catch (_e) {
      // ignore: response already gone
    }
  }
}

function readBody(req) {
  return new Promise((resolve) => {
    let data = '';
    req.on('data', (chunk) => {
      data += chunk;
      if (data.length > 1e6) {
        req.destroy();
      }
    });
    req.on('end', () => resolve(data));
    req.on('error', () => resolve(''));
  });
}

function nextId(collection) {
  let max = 0;
  for (const item of collection) {
    if (typeof item.id === 'number' && item.id > max) max = item.id;
  }
  return max + 1;
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

function sendHealth(req, res) {
  sendJson(res, 200, {
    status: 'ok',
    app: db.meta.appName,
    version: db.meta.version,
    time: new Date().toISOString(),
  });
}

function sendApiRoot(req, res) {
  sendJson(res, 200, {
    message: 'BookStack API',
    endpoints: {
      '/api/health': 'GET - service health',
      '/api/shelves': 'GET/POST - list or create shelves',
      '/api/books': 'GET/POST - list or create books',
      '/api/pages': 'GET - list pages',
      '/api/drafts': 'GET/POST - list or save drafts',
    },
  });
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

function parseCookies(req) {
  const header = req.headers.cookie || '';
  const out = {};
  for (const part of header.split(';')) {
    const idx = part.indexOf('=');
    if (idx === -1) continue;
    const key = part.slice(0, idx).trim();
    const value = part.slice(idx + 1).trim();
    out[key] = value;
  }
  return out;
}

function sessionUser(req) {
  const cookies = parseCookies(req);
  const token = cookies[SESSION_COOKIE];
  if (!token || !sessions.has(token)) return null;
  return sessions.get(token);
}

function createSessionToken() {
  return crypto.randomBytes(24).toString('hex');
}

// Server-renders the top-right area of the header: nickname + Sign out when
// signed in, or the anonymous Login link otherwise.
function headerRightHtml(req) {
  const user = sessionUser(req);
  if (user) {
    return (
      '<span class="nav-link user-name">' + escapeHtml(user) + '</span>' +
      '<a class="nav-link login-link" href="/logout">Sign out</a>'
    );
  }
  return '<a class="nav-link login-link" href="/login">Login</a>';
}

function escapeHtml(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

// Replace the placeholder token in an HTML page body with the correct header
// right-side content.
function renderWithHeader(body, req) {
  return body.split('<!--__HEADER_RIGHT__-->').join(headerRightHtml(req));
}

// Build the shelf list markup (server-rendered from the in-memory store).
function shelfListHtml() {
  if (!db.shelves.length) {
    return '<p class="empty-note">No shelves have been created yet.</p>';
  }
  const items = db.shelves
    .map(function (shelf) {
      return (
        '<li class="shelf-item">' +
        '<a class="shelf-link" href="/shelves/' + encodeURIComponent(shelf.id) + '">' +
        escapeHtml(shelf.name) +
        '</a>' +
        (shelf.description ? '<p class="shelf-desc">' + escapeHtml(shelf.description) + '</p>' : '') +
        '</li>'
      );
    })
    .join('');
  return '<ul class="shelf-list">' + items + '</ul>';
}

// Build the book list markup (server-rendered from the in-memory store).
function bookListHtml() {
  if (!db.books.length) {
    return '<p class="empty-note">No books have been created yet.</p>';
  }
  const items = db.books
    .map(function (book) {
      return (
        '<article class="book-card">' +
        '<a class="book-link" href="/books/' + encodeURIComponent(book.id) + '">' +
        escapeHtml(book.name) +
        '</a>' +
        (book.description ? '<p class="book-desc">' + escapeHtml(book.description) + '</p>' : '') +
        '</article>'
      );
    })
    .join('');
  return '<div class="book-grid">' + items + '</div>';
}

// Build the recent drafts list markup (server-rendered from the in-memory
// store). Each draft links to its book so a saved draft can be revisited.
function recentDraftsHtml() {
  if (!db.drafts.length) {
    return '<p class="empty-note">No recent drafts.</p>';
  }
  const items = db.drafts.map(function (draft) {
    return (
      '<li class="draft-item">' +
      '<a class="draft-link" href="/books/' + encodeURIComponent(draft.bookId) + '">' +
      escapeHtml(draft.name) +
      '</a>' +
      '</li>'
    );
  }).join('');
  return '<ul class="draft-list">' + items + '</ul>';
}

// Build the list of pages belonging to a book (server-rendered).
function pageListHtml(bookId) {
  const pages = db.pages.filter(function (page) {
    return String(page.bookId) === String(bookId);
  });
  const drafts = db.drafts.filter(function (draft) {
    return String(draft.bookId) === String(bookId);
  });
  const items = pages.map(function (page) {
    return (
      '<li class="page-item">' +
      '<a class="page-link" href="/books/' + encodeURIComponent(bookId) + '/pages/' + encodeURIComponent(page.id) + '">' +
      escapeHtml(page.name) +
      '</a>' +
      '</li>'
    );
  });
  const draftItems = drafts.map(function (draft) {
    return (
      '<li class="page-item draft-item">' +
      '<a class="page-link" href="/books/' + encodeURIComponent(bookId) + '/draft/' + encodeURIComponent(draft.id) + '">' +
      escapeHtml(draft.name) +
      '</a>' +
      '</li>'
    );
  });
  if (!items.length && !draftItems.length) {
    return '<p class="empty-note">No pages have been created yet.</p>';
  }
  return '<ul class="page-list">' + items.join('') + draftItems.join('') + '</ul>';
}

// Build the list of chapters belonging to a book (server-rendered).
function chapterListHtml(bookId) {
  const chapters = db.chapters.filter(function (chapter) {
    return String(chapter.bookId) === String(bookId);
  });
  const items = chapters.map(function (chapter) {
    return (
      '<li class="chapter-item">' +
      '<span class="chapter-link">' +
      escapeHtml(chapter.name) +
      '</span>' +
      (chapter.description ? '<p class="chapter-desc">' + escapeHtml(chapter.description) + '</p>' : '') +
      '</li>'
    );
  });
  if (!items.length) {
    return '<p class="empty-note">No chapters have been created yet.</p>';
  }
  return '<ul class="chapter-list">' + items.join('') + '</ul>';
}

// Record an item (shelf, book, or page) as recently viewed. Keeps newest items
// at the front of the list and preserves insertion order for duplicates.
function recordRecentlyViewed(name, href) {
  for (let i = 0; i < recentlyViewed.length; i++) {
    if (recentlyViewed[i].href === href) {
      recentlyViewed.splice(i, 1);
      break;
    }
  }
  recentlyViewed.unshift({ name: name, href: href });
  if (recentlyViewed.length > 20) {
    recentlyViewed.length = 20;
  }
}

// Build the "Recently Updated Pages" list markup (server-rendered from the
// in-memory store). Pages are sorted by newest update/creation first, and each
// item links to its reading page so clicking it opens the page directly.
function recentlyUpdatedPagesHtml() {
  if (!db.pages.length) {
    return '<p class="empty-note">No recently updated pages.</p>';
  }
  const sorted = db.pages.slice().sort(function (a, b) {
    return String(b.createdAt || '').localeCompare(String(a.createdAt || ''));
  });
  const items = sorted.map(function (page) {
    return (
      '<li class="recently-updated-item">' +
      '<a class="recently-updated-link" href="/books/' + encodeURIComponent(page.bookId) + '/pages/' + encodeURIComponent(page.id) + '">' +
      escapeHtml(page.name) +
      '</a>' +
      '</li>'
    );
  }).join('');
  return '<ul class="recently-updated-list">' + items + '</ul>';
}

// Build the "My Recently Viewed" list markup (server-rendered from the
// in-memory list so returning home needs no XHR).
function recentlyViewedHtml() {
  if (!recentlyViewed.length) {
    return '<p class="empty-note">No recently viewed items.</p>';
  }
  const items = recentlyViewed.map(function (item) {
    return (
      '<li class="recently-viewed-item">' +
      '<a class="recently-viewed-link" href="' + escapeHtml(item.href) + '">' +
      escapeHtml(item.name) +
      '</a>' +
      '</li>'
    );
  }).join('');
  return '<ul class="recently-viewed-list">' + items + '</ul>';
}

// A favorite is stored as a string key like "book:<id>", "page:<id>", or
// "shelf:<id>". Kept in the persisted store so the state survives a reload and
// a fresh start reproduces the seed (an empty favorites list).
function isFavorite(key) {
  return (db.favorites || []).indexOf(key) !== -1;
}

function toggleFavorite(key) {
  if (!db.favorites) db.favorites = [];
  const idx = db.favorites.indexOf(key);
  if (idx === -1) {
    db.favorites.push(key);
  } else {
    db.favorites.splice(idx, 1);
  }
  persistDb();
  return isFavorite(key);
}

// Build the "My Most Viewed Favorites" list markup (server-rendered from the
// in-memory store so returning home needs no XHR).
function favoritesHtml() {
  const keys = db.favorites || [];
  if (!keys.length) {
    return '<p class="empty-note">No favorites yet.</p>';
  }
  const items = [];
  for (const key of keys) {
    const parts = String(key).split(':');
    const type = parts[0];
    const id = parts[1];
    let href = null;
    let name = null;
    if (type === 'book') {
      const book = findBookById(id);
      if (book) { href = '/books/' + encodeURIComponent(book.id); name = book.name; }
    } else if (type === 'page') {
      const page = findPageById(id);
      if (page) {
        href = '/books/' + encodeURIComponent(page.bookId) + '/pages/' + encodeURIComponent(page.id);
        name = page.name;
      }
    } else if (type === 'shelf') {
      const shelf = findShelfById(id);
      if (shelf) { href = '/shelves/' + encodeURIComponent(shelf.id); name = shelf.name; }
    }
    if (href && name) {
      items.push(
        '<li class="favorite-item">' +
        '<a class="favorite-link" href="' + escapeHtml(href) + '">' +
        escapeHtml(name) +
        '</a>' +
        '</li>'
      );
    }
  }
  if (!items.length) {
    return '<p class="empty-note">No favorites yet.</p>';
  }
  return '<ul class="favorites-list">' + items.join('') + '</ul>';
}

// Build the Favorite/Unfavorite toggle action for a content item. It is a real
// <button> (matches the spec's "clicks Favorite") and toggles the item's
// favorited state via a synchronous POST that redirects back to the same view.
function favoriteActionHtml(type, id, actionUrl) {
  const key = type + ':' + id;
  const favorited = isFavorite(key);
  const label = favorited ? 'Unfavorite' : 'Favorite';
  return (
    '<form class="favorite-form" action="' + escapeHtml(actionUrl) + '" method="post">' +
    '<button type="submit" class="action-btn favorite-btn">' + label + '</button>' +
    '</form>'
  );
}

// Toggle the favorited state of a content item and redirect back to the view
// it was toggled from. Persists synchronously and does no heavy crypto, so it
// stays well within the CPU budget.
function handleToggleFavorite(res, type, id, redirectUrl) {
  toggleFavorite(type + ':' + id);
  res.writeHead(302, { Location: redirectUrl });
  res.end();
}

// Server-render the homepage, injecting the recent drafts list into the
// "My Recent Drafts" section so a saved draft is visible right after returning
// home without any XHR.
function sendHomePage(req, res) {
  const filePath = path.join(FRONTEND_DIST, 'index.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.replace('<!--__RECENT_DRAFTS__-->', recentDraftsHtml());
    html = html.replace('<!--__RECENTLY_VIEWED__-->', recentlyViewedHtml());
    html = html.replace('<!--__FAVORITES__-->', favoritesHtml());
    html = html.replace('<!--__RECENTLY_UPDATED__-->', recentlyUpdatedPagesHtml());
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

function sendBookListPage(req, res) {
  const filePath = path.join(FRONTEND_DIST, 'books', 'index.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.replace('<!--__BOOK_LIST__-->', bookListHtml());
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Build the book details page for a single book (server-rendered from the
// in-memory store). The book name appears exactly once as the page heading.
function sendBookDetailsPage(req, res, id) {
  let book = null;
  for (const candidate of db.books) {
    if (String(candidate.id) === id) {
      book = candidate;
      break;
    }
  }
  if (!book) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  recordRecentlyViewed(book.name, '/books/' + encodeURIComponent(book.id));
  const filePath = path.join(FRONTEND_DIST, 'books', 'detail.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__BOOK_NAME__').join(escapeHtml(book.name));
    html = html.split('__BOOK_DESC__').join(escapeHtml(book.description || ''));
    html = html.split('__BOOK_CREATED__').join(escapeHtml(String(book.createdAt || '')));
    html = html.split('__EDIT_HREF__').join('/books/' + encodeURIComponent(book.id) + '/edit');
    html = html.split('__DELETE_HREF__').join('/books/' + encodeURIComponent(book.id) + '/delete');
    html = html.split('__NEW_PAGE_HREF__').join('/books/' + encodeURIComponent(book.id) + '/pages/new');
    html = html.split('__NEW_CHAPTER_HREF__').join('/books/' + encodeURIComponent(book.id) + '/chapters/new');
    html = html.split('<!--__CHAPTER_LIST__-->').join(chapterListHtml(book.id));
    html = html.split('<!--__PAGE_LIST__-->').join(pageListHtml(book.id));
    html = html.split('<!--__FAVORITE_ACTION__-->').join(
      favoriteActionHtml('book', book.id, '/books/' + encodeURIComponent(book.id) + '/favorite')
    );
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

function findBookById(id) {
  for (const candidate of db.books) {
    if (String(candidate.id) === id) return candidate;
  }
  return null;
}

// Serve the "Edit Book" page (a separate HTML document) with the current
// book values pre-filled into the form.
function sendEditBookPage(req, res, id) {
  const book = findBookById(id);
  if (!book) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const filePath = path.join(FRONTEND_DIST, 'books', 'edit.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__BOOK_NAME__').join(escapeHtml(book.name));
    html = html.split('__BOOK_DESC__').join(escapeHtml(book.description || ''));
    html = html.split('__BOOK_TAGS__').join(escapeHtml((book.tags || []).join(', ')));
    const editHref = '/books/' + encodeURIComponent(book.id) + '/edit';
    html = html.split('__EDIT_HREF__').join(editHref);
    html = html.split('__CANCEL_HREF__').join('/books/' + encodeURIComponent(book.id));
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Handle saving book edits: update the matching book in the in-memory store,
// persist synchronously, then redirect back to the book details page.
async function handleEditBook(req, res, id) {
  const book = findBookById(id);
  if (!book) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const body = await readBody(req);
  let data = {};
  try {
    const parsed = body ? JSON.parse(body) : {};
    if (parsed && typeof parsed === 'object') data = parsed;
  } catch (e) {
    data = {};
    const params = new URLSearchParams(body || '');
    for (const [key, value] of params.entries()) {
      data[key] = value;
    }
  }
  const name = String(data.name || '').trim();
  if (!name) {
    res.writeHead(400, { 'Content-Type': 'application/json; charset=utf-8' });
    return res.end(JSON.stringify({ error: 'Name is required.' }));
  }
  const description = String(data.description || '').trim();
  const tags = typeof data.tags === 'string'
    ? data.tags.split(',').map(function (t) { return t.trim(); }).filter(Boolean)
    : (Array.isArray(data.tags) ? data.tags : []);

  book.name = name;
  book.description = description;
  book.tags = tags;
  persistDb();
  res.writeHead(302, { Location: '/books/' + encodeURIComponent(book.id) });
  res.end();
}

// Server-renders the delete-confirmation page for a single book. A separate
// HTML document per route keeps every echoed value in exactly one element.
function sendDeleteBookPage(req, res, id) {
  const book = findBookById(id);
  if (!book) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const deleteHref = '/books/' + encodeURIComponent(book.id) + '/delete';
  const cancelHref = '/books/' + encodeURIComponent(book.id);
  const filePath = path.join(FRONTEND_DIST, 'books', 'delete.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__BOOK_NAME__').join(escapeHtml(book.name));
    html = html.split('__DELETE_HREF__').join(deleteHref);
    html = html.split('__CANCEL_HREF__').join(cancelHref);
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Serve the "New Page" creation page for a book (a separate HTML document).
function sendNewPagePage(req, res, id) {
  const book = findBookById(id);
  if (!book) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const filePath = path.join(FRONTEND_DIST, 'books', 'pages', 'new.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__BOOK_ID__').join(escapeHtml(String(book.id)));
    html = html.split('__CANCEL_HREF__').join('/books/' + encodeURIComponent(book.id));
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Serve the "New Chapter" creation page for a book (a separate HTML document).
function sendNewChapterPage(req, res, id) {
  const book = findBookById(id);
  if (!book) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const filePath = path.join(FRONTEND_DIST, 'books', 'chapters', 'new.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__BOOK_ID__').join(escapeHtml(String(book.id)));
    html = html.split('__CANCEL_HREF__').join('/books/' + encodeURIComponent(book.id));
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

function findPageById(id) {
  for (const candidate of db.pages) {
    if (String(candidate.id) === id) return candidate;
  }
  return null;
}

// Serve the "Page Reading Page" for a single page (a separate HTML document).
// Server-renders the page name as the heading and the page content so the page
// needs no XHR after load.
function sendPageReadingPage(req, res, bookId, pageId) {
  const book = findBookById(bookId);
  const page = findPageById(pageId);
  if (!book || !page || String(page.bookId) !== String(book.id)) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  recordRecentlyViewed(page.name, '/books/' + encodeURIComponent(book.id) + '/pages/' + encodeURIComponent(page.id));
  const filePath = path.join(FRONTEND_DIST, 'books', 'pages', 'read.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__BOOK_NAME__').join(escapeHtml(book.name));
    html = html.split('__PAGE_NAME__').join(escapeHtml(page.name));
    html = html.split('__PAGE_CONTENT__').join(escapeHtml(page.content || ''));
    html = html.split('__EDIT_HREF__').join(
      '/books/' + encodeURIComponent(book.id) + '/pages/' + encodeURIComponent(page.id) + '/edit'
    );
    html = html.split('<!--__FAVORITE_ACTION__-->').join(
      favoriteActionHtml(
        'page',
        page.id,
        '/books/' + encodeURIComponent(book.id) + '/pages/' + encodeURIComponent(page.id) + '/favorite'
      )
    );
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Serve the "Page Edit Page" for a single page (a separate HTML document).
// Pre-fills the existing page values (Name, Content) into the form.
function sendPageEditPage(req, res, bookId, pageId) {
  const book = findBookById(bookId);
  const page = findPageById(pageId);
  if (!book || !page || String(page.bookId) !== String(book.id)) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const filePath = path.join(FRONTEND_DIST, 'books', 'pages', 'edit.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__PAGE_NAME__').join(escapeHtml(page.name));
    html = html.split('__PAGE_CONTENT__').join(escapeHtml(page.content || ''));
    html = html.split('__CANCEL_HREF__').join(
      '/books/' + encodeURIComponent(book.id) + '/pages/' + encodeURIComponent(page.id)
    );
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

function findDraftById(id) {
  for (const candidate of db.drafts) {
    if (String(candidate.id) === id) return candidate;
  }
  return null;
}

// Serve the "Edit Draft" page (a separate HTML document) pre-filled with the
// existing draft values. This is the page edit page for an existing draft.
function sendDraftEditPage(req, res, bookId, draftId) {
  const book = findBookById(bookId);
  const draft = findDraftById(draftId);
  if (!book || !draft || String(draft.bookId) !== String(book.id)) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const filePath = path.join(FRONTEND_DIST, 'books', 'pages', 'draft-edit.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__BOOK_ID__').join(escapeHtml(String(book.id)));
    html = html.split('__DRAFT_NAME__').join(escapeHtml(draft.name));
    html = html.split('__DRAFT_CONTENT__').join(escapeHtml(draft.content || ''));
    html = html.split('__CANCEL_HREF__').join('/books/' + encodeURIComponent(book.id));
    html = html.split('__DELETE_HREF__').join(
      '/books/' + encodeURIComponent(book.id) + '/draft/' + encodeURIComponent(draft.id) + '/delete'
    );
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Serve the draft delete-confirmation page (a separate HTML document).
function sendDraftDeletePage(req, res, bookId, draftId) {
  const book = findBookById(bookId);
  const draft = findDraftById(draftId);
  if (!book || !draft || String(draft.bookId) !== String(book.id)) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const deleteHref = '/books/' + encodeURIComponent(book.id) + '/draft/' + encodeURIComponent(draft.id) + '/delete';
  const cancelHref = '/books/' + encodeURIComponent(book.id);
  const filePath = path.join(FRONTEND_DIST, 'books', 'pages', 'draft-delete.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__DRAFT_NAME__').join(escapeHtml(draft.name));
    html = html.split('__DELETE_HREF__').join(deleteHref);
    html = html.split('__CANCEL_HREF__').join(cancelHref);
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Handle the confirmed draft deletion: remove the draft from the in-memory
// store, persist synchronously, then redirect to the related book details page.
function handleDeleteDraft(req, res, bookId, draftId) {
  const book = findBookById(bookId);
  const index = db.drafts.findIndex(function (candidate) {
    return String(candidate.id) === draftId && String(candidate.bookId) === String(bookId);
  });
  if (!book || index === -1) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  db.drafts.splice(index, 1);
  persistDb();
  res.writeHead(302, { Location: '/books/' + encodeURIComponent(book.id) });
  res.end();
}

// Handle the confirmed deletion: remove the book from the in-memory store,
// persist synchronously, then redirect to the books list page.
async function handleDeleteBook(req, res, id) {
  const index = db.books.findIndex(function (candidate) {
    return String(candidate.id) === id;
  });
  if (index === -1) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  db.books.splice(index, 1);
  persistDb();
  res.writeHead(302, { Location: '/books' });
  res.end();
}

function sendShelfListPage(req, res) {
  const filePath = path.join(FRONTEND_DIST, 'shelves', 'index.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.replace('<!--__SHELF_LIST__-->', shelfListHtml());
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Build the list of books belonging to a shelf (server-rendered links that
// lead to each book's details page).
function shelfBooksHtml(shelfId) {
  const books = db.books.filter(function (book) {
    return String(book.shelfId) === String(shelfId);
  });
  if (!books.length) {
    return '<p class="empty-note">No books have been added to this shelf yet.</p>';
  }
  const items = books.map(function (book) {
    return (
      '<li class="shelf-book-item">' +
      '<a class="book-link" href="/books/' + encodeURIComponent(book.id) + '">' +
      escapeHtml(book.name) +
      '</a>' +
      (book.description ? '<p class="book-desc">' + escapeHtml(book.description) + '</p>' : '') +
      '</li>'
    );
  }).join('');
  return '<ul class="shelf-books-list">' + items + '</ul>';
}

// Build the shelf details page for a single shelf (server-rendered from the
// in-memory store). The shelf name appears exactly once as the page heading.
function sendShelfDetailsPage(req, res, id) {
  let shelf = null;
  for (const candidate of db.shelves) {
    if (String(candidate.id) === id) {
      shelf = candidate;
      break;
    }
  }
  if (!shelf) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  recordRecentlyViewed(shelf.name, '/shelves/' + encodeURIComponent(shelf.id));
  const filePath = path.join(FRONTEND_DIST, 'shelves', 'detail.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__SHELF_NAME__').join(escapeHtml(shelf.name));
    html = html.split('__SHELF_DESC__').join(escapeHtml(shelf.description || ''));
    html = html.split('__DELETE_HREF__').join('/shelves/' + encodeURIComponent(shelf.id) + '/delete');
    html = html.split('__EDIT_HREF__').join('/shelves/' + encodeURIComponent(shelf.id) + '/edit');
    html = html.split('__NEW_BOOK_HREF__').join('/shelves/' + encodeURIComponent(shelf.id) + '/new');
    html = html.split('<!--__SHELF_BOOKS__-->').join(shelfBooksHtml(shelf.id));
    html = html.split('<!--__FAVORITE_ACTION__-->').join(
      favoriteActionHtml('shelf', shelf.id, '/shelves/' + encodeURIComponent(shelf.id) + '/favorite')
    );
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

function findShelfById(id) {
  for (const candidate of db.shelves) {
    if (String(candidate.id) === id) return candidate;
  }
  return null;
}

// Server-renders the delete-confirmation page for a single shelf. A separate
// HTML document per route keeps every echoed value in exactly one element.
function sendDeleteShelfPage(req, res, id) {
  const shelf = findShelfById(id);
  if (!shelf) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const deleteHref = '/shelves/' + encodeURIComponent(shelf.id) + '/delete';
  const cancelHref = '/shelves/' + encodeURIComponent(shelf.id);
  const filePath = path.join(FRONTEND_DIST, 'shelves', 'delete.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__SHELF_NAME__').join(escapeHtml(shelf.name));
    html = html.split('__DELETE_HREF__').join(deleteHref);
    html = html.split('__CANCEL_HREF__').join(cancelHref);
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Handle the confirmed deletion: remove the shelf from the in-memory store,
// persist synchronously, then redirect to the shelf list page.
async function handleDeleteShelf(req, res, id) {
  const index = db.shelves.findIndex(function (candidate) {
    return String(candidate.id) === id;
  });
  if (index === -1) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  db.shelves.splice(index, 1);
  persistDb();
  res.writeHead(302, { Location: '/shelves' });
  res.end();
}

async function handleLogin(req, res) {
  const body = await readBody(req);
  let data = {};
  try {
    const parsed = body ? JSON.parse(body) : {};
    if (parsed && typeof parsed === 'object') data = parsed;
  } catch (e) {
    // Not JSON — assume form-urlencoded.
    data = {};
    const params = new URLSearchParams(body || '');
    for (const [key, value] of params.entries()) {
      data[key] = value;
    }
  }
  const email = String(data.email || '').trim().toLowerCase();
  const password = String(data.password || '');
  const user = db.user;

  if (!email || !password) {
    return sendLoginPage(res, req, 'Email and password are required.');
  }
  if (email !== String(user.email).toLowerCase()) {
    return sendLoginPage(res, req, 'Invalid email or password.');
  }
  const parts = String(user.passwordHash || '').split(':');
  const salt = parts[0] || '';
  const storedHash = parts[1] || '';
  const computedHash = crypto
    .scryptSync(password, salt, 64, { N: 4096, r: 8, p: 1 })
    .toString('hex');
  // Timing-safe comparison.
  const a = Buffer.from(computedHash, 'hex');
  const b = Buffer.from(storedHash, 'hex');
  const ok = a.length === b.length && crypto.timingSafeEqual(a, b);

  if (!ok) {
    return sendLoginPage(res, req, 'Invalid email or password.');
  }

  const token = createSessionToken();
  sessions.set(token, user.nickname);
  res.writeHead(302, {
    Location: '/',
    'Set-Cookie':
      SESSION_COOKIE + '=' + token + '; Path=/; HttpOnly; SameSite=Lax; Max-Age=' + SESSION_MAX_AGE,
  });
  res.end();
}

function sendLoginPage(res, req, error) {
  const filePath = path.join(FRONTEND_DIST, 'login', 'index.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    if (error) {
      html = html.replace(
        '<!--__ERROR__-->',
        '<div class="form-error" role="alert">' + escapeHtml(error) + '</div>'
      );
    }
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

function handleLogout(req, res) {
  const cookies = parseCookies(req);
  const token = cookies[SESSION_COOKIE];
  if (token) sessions.delete(token);
  res.writeHead(302, {
    Location: '/',
    'Set-Cookie':
      SESSION_COOKIE + '=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0',
  });
  res.end();
}

// Serve the "New Shelf" creation page (a separate HTML document).
function sendNewShelfPage(req, res) {
  const filePath = path.join(FRONTEND_DIST, 'shelves', 'new.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Serve the "Edit Shelf" page (a separate HTML document).
function sendEditShelfPage(req, res, id) {
  const shelf = findShelfById(id);
  if (!shelf) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const filePath = path.join(FRONTEND_DIST, 'shelves', 'edit.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    let html = content.toString('utf8');
    html = html.split('__SHELF_NAME__').join(escapeHtml(shelf.name));
    html = html.split('__SHELF_DESC__').join(escapeHtml(shelf.description || ''));
    html = html.split('__SHELF_TAGS__').join(escapeHtml((shelf.tags || []).join(', ')));
    const editHref = '/shelves/' + encodeURIComponent(shelf.id) + '/edit';
    html = html.split('__EDIT_HREF__').join(editHref);
    html = html.split('__CANCEL_HREF__').join('/shelves/' + encodeURIComponent(shelf.id));
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Handle saving shelf edits: update the matching shelf in the in-memory store,
// persist synchronously, then redirect back to the shelf details page.
async function handleEditShelf(req, res, id) {
  const shelf = findShelfById(id);
  if (!shelf) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const body = await readBody(req);
  let data = {};
  try {
    const parsed = body ? JSON.parse(body) : {};
    if (parsed && typeof parsed === 'object') data = parsed;
  } catch (e) {
    data = {};
    const params = new URLSearchParams(body || '');
    for (const [key, value] of params.entries()) {
      data[key] = value;
    }
  }
  const name = String(data.name || '').trim();
  if (!name) {
    res.writeHead(400, { 'Content-Type': 'application/json; charset=utf-8' });
    return res.end(JSON.stringify({ error: 'Name is required.' }));
  }
  const description = String(data.description || '').trim();
  const tags = typeof data.tags === 'string'
    ? data.tags.split(',').map(function (t) { return t.trim(); }).filter(Boolean)
    : (Array.isArray(data.tags) ? data.tags : []);

  shelf.name = name;
  shelf.description = description;
  shelf.tags = tags;
  persistDb();
  res.writeHead(302, { Location: '/shelves/' + encodeURIComponent(shelf.id) });
  res.end();
}

// Serve the "New Book" creation page for a given shelf (a separate HTML
// document). The shelf name is echoed in the breadcrumb and tagline so the
// user has context of the current shelf.
function sendNewShelfBookPage(req, res, id) {
  const shelf = findShelfById(id);
  if (!shelf) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const filePath = path.join(FRONTEND_DIST, 'shelves', 'new-book.html');
  fs.readFile(filePath, (err, content) => {
    if (err) {
      return sendJson(res, 404, { error: 'Not Found' });
    }
    const shelfHref = '/shelves/' + encodeURIComponent(shelf.id);
    let html = content.toString('utf8');
    html = html.split('__SHELF_HREF__').join(shelfHref);
    html = html.split('__SHELF_NAME__').join(escapeHtml(shelf.name));
    html = html.split('__NEW_BOOK_HREF__').join(shelfHref + '/new');
    html = renderWithHeader(html, req);
    const body = Buffer.from(html, 'utf8');
    res.writeHead(200, {
      'Content-Type': 'text/html; charset=utf-8',
      'Content-Length': Buffer.byteLength(body),
    });
    res.end(body);
  });
}

// Handle creating a book from the "New Book in shelf" page. Validates that a
// name is provided, creates the book associated with the current shelf,
// persists synchronously, then redirects back to the shelf details page (where
// the new book appears in the shelf's book list).
async function handleCreateShelfBook(req, res, id) {
  const shelf = findShelfById(id);
  if (!shelf) {
    return sendJson(res, 404, { error: 'Not Found' });
  }
  const body = await readBody(req);
  let data = {};
  try {
    const parsed = body ? JSON.parse(body) : {};
    if (parsed && typeof parsed === 'object') data = parsed;
  } catch (e) {
    data = {};
    const params = new URLSearchParams(body || '');
    for (const [key, value] of params.entries()) {
      data[key] = value;
    }
  }
  const name = String(data.name || '').trim();
  if (!name) {
    res.writeHead(400, { 'Content-Type': 'application/json; charset=utf-8' });
    return res.end(JSON.stringify({ error: 'Name is required.' }));
  }
  const description = String(data.description || '').trim();
  const tags = typeof data.tags === 'string'
    ? data.tags.split(',').map(function (t) { return t.trim(); }).filter(Boolean)
    : (Array.isArray(data.tags) ? data.tags : []);

  const book = {
    id: nextId(db.books),
    name: name,
    description: description,
    tags: tags,
    shelfId: shelf.id,
    createdAt: new Date().toISOString(),
  };
  db.books.push(book);
  persistDb();
  res.writeHead(302, { Location: '/shelves/' + encodeURIComponent(shelf.id) });
  res.end();
}

function handleApi(req, res, pathname, method) {
  if (pathname === '/api/health') {
    if (method === 'GET') return sendHealth(req, res);
    return sendJson(res, 405, { error: 'Method Not Allowed' });
  }

  if (pathname === '/api') {
    if (method === 'GET') return sendApiRoot(req, res);
    return sendJson(res, 405, { error: 'Method Not Allowed' });
  }

  if (pathname === '/api/shelves') {
    if (method === 'GET') return sendJson(res, 200, { shelves: db.shelves });
    if (method === 'POST') return handleCreateShelf(req, res);
    return sendJson(res, 405, { error: 'Method Not Allowed' });
  }

  if (pathname === '/api/books') {
    if (method === 'GET') return sendJson(res, 200, { books: db.books });
    if (method === 'POST') return handleCreateBook(req, res);
    return sendJson(res, 405, { error: 'Method Not Allowed' });
  }

  if (pathname === '/api/pages') {
    if (method === 'GET') return sendJson(res, 200, { pages: db.pages });
    if (method === 'POST') return handleCreatePage(req, res);
    return sendJson(res, 405, { error: 'Method Not Allowed' });
  }

  if (pathname === '/api/drafts') {
    if (method === 'GET') return sendJson(res, 200, { drafts: db.drafts });
    if (method === 'POST') return handleCreateDraft(req, res);
    return sendJson(res, 405, { error: 'Method Not Allowed' });
  }

  if (pathname === '/api/chapters') {
    if (method === 'GET') return sendJson(res, 200, { chapters: db.chapters });
    if (method === 'POST') return handleCreateChapter(req, res);
    return sendJson(res, 405, { error: 'Method Not Allowed' });
  }

  // Unknown /api route
  return sendJson(res, 404, { error: 'Not Found' });
}

async function handleCreateShelf(req, res) {
  const body = await readBody(req);
  let data = {};
  try {
    data = body ? JSON.parse(body) : {};
  } catch (e) {
    return sendJson(res, 400, { error: 'Invalid JSON body' });
  }
  const shelf = {
    id: nextId(db.shelves),
    name: data.name || 'Untitled Shelf',
    description: data.description || '',
    tags: data.tags || [],
    createdAt: new Date().toISOString(),
  };
  db.shelves.push(shelf);
  persistDb();
  return sendJson(res, 201, { shelf });
}

async function handleCreateBook(req, res) {
  const body = await readBody(req);
  let data = {};
  try {
    data = body ? JSON.parse(body) : {};
  } catch (e) {
    return sendJson(res, 400, { error: 'Invalid JSON body' });
  }
  const book = {
    id: nextId(db.books),
    name: data.name || 'Untitled Book',
    description: data.description || '',
    tags: data.tags || [],
    shelfId: data.shelfId || null,
    createdAt: new Date().toISOString(),
  };
  db.books.push(book);
  persistDb();
  return sendJson(res, 201, { book });
}

async function handleCreatePage(req, res) {
  const body = await readBody(req);
  let data = {};
  try {
    const parsed = body ? JSON.parse(body) : {};
    if (parsed && typeof parsed === 'object') data = parsed;
  } catch (e) {
    data = {};
    const params = new URLSearchParams(body || '');
    for (const [key, value] of params.entries()) {
      data[key] = value;
    }
  }
  const name = String(data.name || '').trim();
  if (!name) {
    res.writeHead(400, { 'Content-Type': 'application/json; charset=utf-8' });
    return res.end(JSON.stringify({ error: 'Name is required.' }));
  }
  const bookId = data.bookId != null ? Number(data.bookId) : null;
  const page = {
    id: nextId(db.pages),
    bookId: bookId,
    name: name,
    content: String(data.content || ''),
    createdAt: new Date().toISOString(),
  };
  db.pages.push(page);
  persistDb();
  return sendJson(res, 201, { page });
}

// Create a chapter within a book: validate that a name is provided, create the
// chapter record in the in-memory store, persist synchronously, and return it.
async function handleCreateChapter(req, res) {
  const body = await readBody(req);
  let data = {};
  try {
    const parsed = body ? JSON.parse(body) : {};
    if (parsed && typeof parsed === 'object') data = parsed;
  } catch (e) {
    data = {};
    const params = new URLSearchParams(body || '');
    for (const [key, value] of params.entries()) {
      data[key] = value;
    }
  }
  const name = String(data.name || '').trim();
  if (!name) {
    res.writeHead(400, { 'Content-Type': 'application/json; charset=utf-8' });
    return res.end(JSON.stringify({ error: 'Name is required.' }));
  }
  const bookId = data.bookId != null ? Number(data.bookId) : null;
  const chapter = {
    id: nextId(db.chapters),
    bookId: bookId,
    name: name,
    description: String(data.description || ''),
    createdAt: new Date().toISOString(),
  };
  db.chapters.push(chapter);
  persistDb();
  return sendJson(res, 201, { chapter });
}

// Save a draft: validate that a name is provided, create the draft record in
// the in-memory store, persist synchronously, and return the created draft.
async function handleCreateDraft(req, res) {
  const body = await readBody(req);
  let data = {};
  try {
    const parsed = body ? JSON.parse(body) : {};
    if (parsed && typeof parsed === 'object') data = parsed;
  } catch (e) {
    data = {};
    const params = new URLSearchParams(body || '');
    for (const [key, value] of params.entries()) {
      data[key] = value;
    }
  }
  const name = String(data.name || '').trim();
  if (!name) {
    res.writeHead(400, { 'Content-Type': 'application/json; charset=utf-8' });
    return res.end(JSON.stringify({ error: 'Name is required.' }));
  }
  const bookId = data.bookId != null ? Number(data.bookId) : null;
  const draft = {
    id: nextId(db.drafts),
    bookId: bookId,
    name: name,
    content: String(data.content || ''),
    createdAt: new Date().toISOString(),
  };
  db.drafts.push(draft);
  persistDb();
  return sendJson(res, 201, { draft });
}

// Handle creating a book from the "New Book" page (/books/new). Validates that
// a name is provided, creates the book in the in-memory store, persists
// synchronously, then redirects to the new book's details page.
async function handleCreateBookPage(req, res) {
  const body = await readBody(req);
  let data = {};
  try {
    const parsed = body ? JSON.parse(body) : {};
    if (parsed && typeof parsed === 'object') data = parsed;
  } catch (e) {
    data = {};
    const params = new URLSearchParams(body || '');
    for (const [key, value] of params.entries()) {
      data[key] = value;
    }
  }
  const name = String(data.name || '').trim();
  if (!name) {
    res.writeHead(400, { 'Content-Type': 'application/json; charset=utf-8' });
    return res.end(JSON.stringify({ error: 'Name is required.' }));
  }
  const description = String(data.description || '').trim();
  const tags = typeof data.tags === 'string'
    ? data.tags.split(',').map(function (t) { return t.trim(); }).filter(Boolean)
    : (Array.isArray(data.tags) ? data.tags : []);
  const shelfId = data.shelfId != null && data.shelfId !== '' ? data.shelfId : null;

  const book = {
    id: nextId(db.books),
    name: name,
    description: description,
    tags: tags,
    shelfId: shelfId,
    createdAt: new Date().toISOString(),
  };
  db.books.push(book);
  persistDb();
  res.writeHead(302, { Location: '/books/' + encodeURIComponent(book.id) });
  res.end();
}

// ---------------------------------------------------------------------------
// Static file serving
// ---------------------------------------------------------------------------

function serveStatic(req, res, pathname) {
  // Strip query/fragment already handled by url.parse pathname.
  let safePath;
  try {
    safePath = decodeURIComponent(pathname);
  } catch (e) {
    // Malformed encoding in path -> 404, never throw.
    return sendNotFound(res, pathname);
  }

  if (safePath === '/' || safePath === '') {
    safePath = '/index.html';
  }

  // Serve index.html when the path points at a directory.
  const requestedFile = path.join(FRONTEND_DIST, safePath);
  try {
    if (fs.statSync(requestedFile).isDirectory()) {
      safePath = path.join(safePath, 'index.html');
    }
  } catch (e) {
    // ignore: fall through to readFile which will 404 below
  }

  const filePath = path.normalize(path.join(FRONTEND_DIST, safePath));

  // Prevent path traversal outside the dist directory.
  if (!filePath.startsWith(path.normalize(FRONTEND_DIST))) {
    return sendJson(res, 403, { error: 'Forbidden' });
  }

  // Unknown/missing static file (e.g. /favicon.ico): respond with a clean 404
  // immediately and synchronously. This guarantees the client always receives an
  // HTTP response instead of a dropped/reset connection (never throw).
  try {
    if (!fs.statSync(filePath).isFile()) {
      return sendNotFound(res, safePath);
    }
  } catch (e) {
    return sendNotFound(res, safePath);
  }

  fs.readFile(filePath, (err, content) => {
    // Guard the entire callback so any failure still produces a response
    // instead of an abandoned request (ConnectionResetError on the client).
    try {
      if (err) {
        // Missing file -> 404 (JSON), never crash on ENOENT (e.g. /favicon.ico).
        return sendNotFound(res, safePath);
      }
      const ext = path.extname(filePath).toLowerCase();
      const type = {
        '.html': 'text/html; charset=utf-8',
        '.css': 'text/css; charset=utf-8',
        '.js': 'application/javascript; charset=utf-8',
        '.json': 'application/json; charset=utf-8',
        '.png': 'image/png',
        '.jpg': 'image/jpeg',
        '.jpeg': 'image/jpeg',
        '.gif': 'image/gif',
        '.svg': 'image/svg+xml',
        '.ico': 'image/x-icon',
      }[ext] || 'application/octet-stream';

      let payload = content;
      if (type.indexOf('text/html') === 0) {
        payload = Buffer.from(renderWithHeader(content.toString('utf8'), req), 'utf8');
      }
      if (res.headersSent) {
        return res.end();
      }
      res.writeHead(200, { 'Content-Type': type });
      res.end(payload);
    } catch (sendErr) {
      try {
        if (!res.headersSent) {
          return sendJson(res, 500, { error: 'Internal Server Error' });
        }
      } catch (e) {
        // Response already gone; swallow to avoid an uncaught exception.
      }
      try {
        res.end();
      } catch (e) {
        // ignore
      }
    }
  });
}

// ---------------------------------------------------------------------------
// Request handler (fully guarded)
// ---------------------------------------------------------------------------

function createHandler() {
  return function onRequest(req, res) {
    // Wrap EVERY request in try/catch so a failing handler returns 500 JSON
    // and never takes down the process.
    try {
      const parsed = url.parse(req.url || '/', true);
      const pathname = parsed.pathname;
      const method = (req.method || 'GET').toUpperCase();

      if (pathname.startsWith('/api/')) {
        return handleApi(req, res, pathname, method);
      }
      if (pathname === '/api') {
        return handleApi(req, res, pathname, method);
      }

      if (pathname === '/login' && method === 'POST') {
        return handleLogin(req, res);
      }
      if (pathname === '/login' && method === 'GET') {
        return serveStatic(req, res, pathname);
      }
      if (pathname === '/logout' && method === 'GET') {
        return handleLogout(req, res);
      }
      if (pathname === '/' && method === 'GET') {
        return sendHomePage(req, res);
      }
      if (pathname === '/shelves' && method === 'GET') {
        return sendShelfListPage(req, res);
      }
      if (pathname === '/books' && method === 'GET') {
        return sendBookListPage(req, res);
      }
      if (pathname === '/shelves/new' && method === 'GET') {
        return sendNewShelfPage(req, res);
      }

      // /shelves/<id>/new — new-book-from-shelf page (GET) and creation (POST).
      // Must be checked before the /shelves/<id> pattern so "new" is not
      // treated as a shelf id.
      const newBookMatch = pathname.match(/^\/shelves\/([^/]+)\/new$/);
      if (newBookMatch && method === 'GET') {
        return sendNewShelfBookPage(req, res, decodeURIComponent(newBookMatch[1]));
      }
      if (newBookMatch && method === 'POST') {
        return handleCreateShelfBook(req, res, decodeURIComponent(newBookMatch[1]));
      }

      // /shelves/<id>/favorite — toggle favorite for a shelf (POST).
      // Must be checked before the /shelves/<id> pattern.
      const shelfFavoriteMatch = pathname.match(/^\/shelves\/([^/]+)\/favorite$/);
      if (shelfFavoriteMatch && method === 'POST') {
        return handleToggleFavorite(
          res,
          'shelf',
          decodeURIComponent(shelfFavoriteMatch[1]),
          '/shelves/' + decodeURIComponent(shelfFavoriteMatch[1])
        );
      }

      // /shelves/<id> — shelf details page
      const shelfMatch = pathname.match(/^\/shelves\/([^/]+)$/);
      if (shelfMatch && method === 'GET') {
        return sendShelfDetailsPage(req, res, decodeURIComponent(shelfMatch[1]));
      }

      // /shelves/<id>/delete — confirmation page (GET) and confirmed deletion (POST)
      const deleteMatch = pathname.match(/^\/shelves\/([^/]+)\/delete$/);
      if (deleteMatch && method === 'GET') {
        return sendDeleteShelfPage(req, res, decodeURIComponent(deleteMatch[1]));
      }
      if (deleteMatch && method === 'POST') {
        return handleDeleteShelf(req, res, decodeURIComponent(deleteMatch[1]));
      }

      // /shelves/<id>/edit — edit page (GET) and saved edits (POST)
      const editMatch = pathname.match(/^\/shelves\/([^/]+)\/edit$/);
      if (editMatch && method === 'GET') {
        return sendEditShelfPage(req, res, decodeURIComponent(editMatch[1]));
      }
      if (editMatch && method === 'POST') {
        return handleEditShelf(req, res, decodeURIComponent(editMatch[1]));
      }

      // /books/new — new book creation page (GET) and creation (POST).
      // Must be checked before the /books/<id> pattern so "new" is not
      // treated as a numeric book id.
      if (pathname === '/books/new' && method === 'GET') {
        const filePath = path.join(FRONTEND_DIST, 'books', 'new.html');
        return fs.readFile(filePath, (err, content) => {
          if (err) {
            return sendJson(res, 404, { error: 'Not Found' });
          }
          let html = content.toString('utf8');
          html = renderWithHeader(html, req);
          const body = Buffer.from(html, 'utf8');
          res.writeHead(200, {
            'Content-Type': 'text/html; charset=utf-8',
            'Content-Length': Buffer.byteLength(body),
          });
          res.end(body);
        });
      }
      if (pathname === '/books/new' && method === 'POST') {
        return handleCreateBookPage(req, res);
      }

      // /books/<id>/edit — edit page (GET) and saved edits (POST). Must be
      // checked before the /books/<id> pattern so "edit" is not treated as
      // part of a book id.
      const bookEditMatch = pathname.match(/^\/books\/([^/]+)\/edit$/);
      if (bookEditMatch && method === 'GET') {
        return sendEditBookPage(req, res, decodeURIComponent(bookEditMatch[1]));
      }
      if (bookEditMatch && method === 'POST') {
        return handleEditBook(req, res, decodeURIComponent(bookEditMatch[1]));
      }

      // /books/<id>/delete — confirmation page (GET) and confirmed deletion (POST)
      const bookDeleteMatch = pathname.match(/^\/books\/([^/]+)\/delete$/);
      if (bookDeleteMatch && method === 'GET') {
        return sendDeleteBookPage(req, res, decodeURIComponent(bookDeleteMatch[1]));
      }
      if (bookDeleteMatch && method === 'POST') {
        return handleDeleteBook(req, res, decodeURIComponent(bookDeleteMatch[1]));
      }

      // /books/<id>/pages/new — new page creation page (GET).
      // Must be checked before the /books/<id> pattern.
      const newPageMatch = pathname.match(/^\/books\/([^/]+)\/pages\/new$/);
      if (newPageMatch && method === 'GET') {
        return sendNewPagePage(req, res, decodeURIComponent(newPageMatch[1]));
      }

      // /books/<id>/chapters/new — new chapter creation page (GET).
      // Must be checked before the /books/<id> pattern.
      const newChapterMatch = pathname.match(/^\/books\/([^/]+)\/chapters\/new$/);
      if (newChapterMatch && method === 'GET') {
        return sendNewChapterPage(req, res, decodeURIComponent(newChapterMatch[1]));
      }

      // /books/<id>/pages/<pageId>/edit — page edit page (GET).
      // Must be checked before /books/<id> and before the page reading page.
      const pageEditMatch = pathname.match(/^\/books\/([^/]+)\/pages\/([^/]+)\/edit$/);
      if (pageEditMatch && method === 'GET') {
        return sendPageEditPage(req, res, decodeURIComponent(pageEditMatch[1]), decodeURIComponent(pageEditMatch[2]));
      }

      // /books/<id>/pages/<pageId>/favorite — toggle favorite for a page (POST).
      // Must be checked before the page reading page.
      const pageFavoriteMatch = pathname.match(/^\/books\/([^/]+)\/pages\/([^/]+)\/favorite$/);
      if (pageFavoriteMatch && method === 'POST') {
        return handleToggleFavorite(
          res,
          'page',
          decodeURIComponent(pageFavoriteMatch[2]),
          '/books/' + decodeURIComponent(pageFavoriteMatch[1]) + '/pages/' + decodeURIComponent(pageFavoriteMatch[2])
        );
      }

      // /books/<id>/pages/<pageId> — page reading page (GET).
      // Must be checked before /books/<id>.
      const pageReadMatch = pathname.match(/^\/books\/([^/]+)\/pages\/([^/]+)$/);
      if (pageReadMatch && method === 'GET') {
        return sendPageReadingPage(req, res, decodeURIComponent(pageReadMatch[1]), decodeURIComponent(pageReadMatch[2]));
      }

      // /books/<id>/draft/<draftId> — draft edit (page edit) page (GET)
      // /books/<id>/draft/<draftId>/delete — delete confirmation (GET) and
      // confirmed deletion (POST). Must be checked before /books/<id>.
      const draftEditMatch = pathname.match(/^\/books\/([^/]+)\/draft\/([^/]+)$/);
      if (draftEditMatch && method === 'GET') {
        return sendDraftEditPage(req, res, decodeURIComponent(draftEditMatch[1]), decodeURIComponent(draftEditMatch[2]));
      }
      const draftDeleteMatch = pathname.match(/^\/books\/([^/]+)\/draft\/([^/]+)\/delete$/);
      if (draftDeleteMatch && method === 'GET') {
        return sendDraftDeletePage(req, res, decodeURIComponent(draftDeleteMatch[1]), decodeURIComponent(draftDeleteMatch[2]));
      }
      if (draftDeleteMatch && method === 'POST') {
        return handleDeleteDraft(req, res, decodeURIComponent(draftDeleteMatch[1]), decodeURIComponent(draftDeleteMatch[2]));
      }

      // /books/<id>/favorite — toggle favorite for a book (POST).
      // Must be checked before the /books/<id> pattern.
      const bookFavoriteMatch = pathname.match(/^\/books\/([^/]+)\/favorite$/);
      if (bookFavoriteMatch && method === 'POST') {
        return handleToggleFavorite(
          res,
          'book',
          decodeURIComponent(bookFavoriteMatch[1]),
          '/books/' + decodeURIComponent(bookFavoriteMatch[1])
        );
      }

      // /books/<id> — book details page
      const bookMatch = pathname.match(/^\/books\/([^/]+)$/);
      if (bookMatch && method === 'GET') {
        return sendBookDetailsPage(req, res, decodeURIComponent(bookMatch[1]));
      }

      return serveStatic(req, res, pathname);
    } catch (err) {
      try {
        return sendJson(res, 500, { error: 'Internal Server Error' });
      } catch (e) {
        // If the response is already gone, just swallow.
      }
    }
  };
}

// ---------------------------------------------------------------------------
// Crash safety hooks
// ---------------------------------------------------------------------------

process.on('uncaughtException', (err) => {
  console.error('[uncaughtException]', err && err.stack ? err.stack : err);
});

process.on('unhandledRejection', (reason) => {
  console.error('[unhandledRejection]', reason);
});

process.on('SIGINT', () => {
  console.log('\nShutting down...');
  process.exit(0);
});

process.on('SIGTERM', () => {
  console.log('\nShutting down...');
  process.exit(0);
});

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

loadDb();

const server = http.createServer(createHandler());

// Never let a connection-level error kill the request silently or reset the
// socket before responding (the client would see an ECONNRESET / "no HTTP
// response"). Reply with 400 and close cleanly so the client always receives
// a complete HTTP response, and ignore benign resets.
server.on('clientError', (err, socket) => {
  if (err && err.code === 'ECONNRESET') {
    socket.destroy();
    return;
  }
  if (socket.writable) {
    socket.end('HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n');
  } else {
    socket.destroy();
  }
});

server.listen(PORT, () => {
  console.log(`BookStack backend listening on http://127.0.0.1:${PORT}`);
  console.log(`Serving frontend from: ${FRONTEND_DIST}`);
});
