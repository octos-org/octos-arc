/* Keep frontend: take-a-note composer, notes list, and note actions (archive, delete with Undo). */
(function () {
  'use strict';

  var composer = document.getElementById('composer');
  var composerInput = document.getElementById('composer-input');
  var composerEditor = document.getElementById('composer-editor');
  var noteTitle = document.getElementById('note-title');
  var noteContent = document.getElementById('note-content');
  var composerClose = document.getElementById('composer-close');
  var composerPin = document.getElementById('composer-pin');
  var composerError = document.getElementById('composer-error');
  var composerColorBtn = document.getElementById('composer-color');
  var composerPalette = document.getElementById('composer-palette');
  var composerMore = document.getElementById('composer-more');
  var composerMenu = document.getElementById('composer-menu');
  var composerLabelsItem = document.getElementById('composer-labels-item');
  var pinnedContainer = document.getElementById('pinned-notes');
  var otherContainer = document.getElementById('other-notes');
  var pinnedSection = document.getElementById('pinned-section');
  var othersSection = document.getElementById('others-section');
  var archiveSection = document.getElementById('archive-section');
  var archiveContainer = document.getElementById('archive-notes');
  var trashSection = document.getElementById('trash-section');
  var trashContainer = document.getElementById('trash-notes');
  var notification = document.getElementById('notification');
  var sidebar = document.getElementById('sidebar');
  var menuButton = document.getElementById('menu-button');
  var appMain = document.getElementById('app-main');
  var sidebarItems = document.querySelectorAll('.sidebar-item');
  var searchInput = document.getElementById('search-input');
  var searchFilters = document.getElementById('search-filters');
  var settingsButton = document.getElementById('settings-button');
  var settingsMenu = document.getElementById('settings-menu');
  var settingsDialog = document.getElementById('settings-dialog');
  var settingsSave = document.getElementById('settings-save');
  var settingsCancel = document.getElementById('settings-cancel');
  var viewToggle = document.getElementById('view-toggle');

  var notes = [];
  var currentView = 'notes';
  var currentLabel = null;
  var searchFilter = null;
  var searchQuery = null;
  var editingNote = null;
  var sidebarOpen = false;
  var listViewActive = false;
  var composerSelectedColor = null;
  var composerSelectedLabels = [];
  var composerPinned = false;
  var labelDialogNote = null;
  var composerLabelDialog = false;

  function appendWithHighlight(container, text) {
    if (!searchQuery) {
      container.textContent = text;
      return;
    }
    var q = searchQuery.toLowerCase();
    var lower = text.toLowerCase();
    var last = 0;
    var i = lower.indexOf(q);
    if (i === -1) {
      container.textContent = text;
      return;
    }
    while (i !== -1) {
      if (i > last) container.appendChild(document.createTextNode(text.slice(last, i)));
      var mark = document.createElement('mark');
      mark.textContent = text.slice(i, i + searchQuery.length);
      container.appendChild(mark);
      last = i + searchQuery.length;
      i = lower.indexOf(q, last);
    }
    if (last < text.length) container.appendChild(document.createTextNode(text.slice(last)));
  }

  function noteCard(note, inTrash, inArchive) {
    var card = document.createElement('div');
    card.className = 'note-card';
    if (note.color) card.style.background = note.color;

    var body = document.createElement('div');
    body.className = 'note-body';
    body.addEventListener('click', function (event) {
      event.stopPropagation();
      openNoteEditor(note);
    });
    body.addEventListener('keydown', function (event) {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        openNoteEditor(note);
      }
    });
    if (note.title) {
      var title = document.createElement('div');
      title.className = 'note-title';
      appendWithHighlight(title, note.title);
      body.appendChild(title);
    }
    if (note.content) {
      var content = document.createElement('div');
      content.className = 'note-content';
      appendWithHighlight(content, note.content);
      body.appendChild(content);
    }
    if (note.labels && note.labels.length && !inTrash) {
      var labels = document.createElement('div');
      labels.className = 'note-labels';
      note.labels.forEach(function (label) {
        var chip = document.createElement('span');
        chip.className = 'note-label-chip';
        chip.textContent = label;
        labels.appendChild(chip);
      });
      body.appendChild(labels);
    }
    card.appendChild(body);

    if (!inTrash) {
      if (inArchive) {
        var unarchiveBtn = document.createElement('button');
        unarchiveBtn.type = 'button';
        unarchiveBtn.className = 'note-archive';
        unarchiveBtn.setAttribute('aria-label', 'Unarchive');
        unarchiveBtn.title = 'Unarchive';
        unarchiveBtn.textContent = 'Unarchive';
        unarchiveBtn.addEventListener('click', function (event) {
          event.stopPropagation();
          unarchiveNote(note);
          closeMenu(card);
        });
        card.appendChild(unarchiveBtn);
      } else {
        var archiveBtn = document.createElement('button');
        archiveBtn.type = 'button';
        archiveBtn.className = 'note-archive';
        archiveBtn.setAttribute('aria-label', 'Archive');
        archiveBtn.title = 'Archive';
        archiveBtn.textContent = 'Archive';
        archiveBtn.addEventListener('click', function (event) {
          event.stopPropagation();
          archiveNote(note);
          closeMenu(card);
        });
        card.appendChild(archiveBtn);

        var pinBtn = document.createElement('button');
        pinBtn.type = 'button';
        pinBtn.className = 'note-pin';
        if (note.pinned) {
          pinBtn.setAttribute('aria-label', 'Unpin');
          pinBtn.title = 'Unpin';
          pinBtn.textContent = 'Unpin';
        } else {
          pinBtn.setAttribute('aria-label', 'Pin');
          pinBtn.title = 'Pin';
          pinBtn.textContent = 'Pin';
        }
        pinBtn.addEventListener('click', function (event) {
          event.stopPropagation();
          if (note.pinned) {
            unpinNote(note);
          } else {
            pinNote(note);
          }
          closeMenu(card);
        });
        card.appendChild(pinBtn);
      }
    }

    var more = document.createElement('button');
    more.type = 'button';
    more.className = 'note-more';
    more.setAttribute('aria-label', 'More options');
    more.title = 'More options';
    more.innerHTML = '&#8942;';
    card.appendChild(more);

    var colorBtn = document.createElement('button');
    colorBtn.type = 'button';
    colorBtn.className = 'note-color';
    colorBtn.setAttribute('aria-label', 'Background options');
    colorBtn.title = 'Background options';
    colorBtn.textContent = 'Background';
    card.appendChild(colorBtn);

    var palette = document.createElement('div');
    palette.className = 'note-palette';
    palette.hidden = true;

    var greenBtn = document.createElement('button');
    greenBtn.type = 'button';
    greenBtn.className = 'palette-swatch palette-green';
    greenBtn.setAttribute('aria-label', 'Light green');
    greenBtn.title = 'Light green';
    greenBtn.addEventListener('click', function (event) {
      event.stopPropagation();
      changeNoteColor(note, '#ccff90');
      closePalette(card);
    });
    palette.appendChild(greenBtn);

    card.appendChild(palette);

    colorBtn.addEventListener('click', function (event) {
      event.stopPropagation();
      togglePalette(card, palette);
    });

    var menu = document.createElement('div');
    menu.className = 'note-menu';
    menu.hidden = true;

    if (inTrash) {
      var restoreItem = document.createElement('button');
      restoreItem.type = 'button';
      restoreItem.className = 'note-menu-item';
      restoreItem.textContent = 'Restore';
      restoreItem.addEventListener('click', function (event) {
        event.stopPropagation();
        restoreNote(note);
        closeMenu(card);
      });
      menu.appendChild(restoreItem);

      var deleteForeverItem = document.createElement('button');
      deleteForeverItem.type = 'button';
      deleteForeverItem.className = 'note-menu-item';
      deleteForeverItem.textContent = 'Delete Forever';
      deleteForeverItem.addEventListener('click', function (event) {
        event.stopPropagation();
        deleteForever(note);
        closeMenu(card);
      });
      menu.appendChild(deleteForeverItem);
    } else if (inArchive) {
      var unarchiveItem = document.createElement('button');
      unarchiveItem.type = 'button';
      unarchiveItem.className = 'note-menu-item';
      unarchiveItem.textContent = 'Unarchive';
      unarchiveItem.addEventListener('click', function (event) {
        event.stopPropagation();
        unarchiveNote(note);
        closeMenu(card);
      });
      menu.appendChild(unarchiveItem);

      var deleteItem = document.createElement('button');
      deleteItem.type = 'button';
      deleteItem.className = 'note-menu-item';
      deleteItem.textContent = 'Delete Note';
      deleteItem.addEventListener('click', function (event) {
        event.stopPropagation();
        deleteNote(note);
        closeMenu(card);
      });
      menu.appendChild(deleteItem);
    } else {
      var labelsItem = document.createElement('button');
      labelsItem.type = 'button';
      labelsItem.className = 'note-menu-item';
      labelsItem.textContent = 'Change labels';
      labelsItem.addEventListener('click', function (event) {
        event.stopPropagation();
        openLabelDialog(note);
        closeMenu(card);
      });
      menu.appendChild(labelsItem);

      var deleteItem2 = document.createElement('button');
      deleteItem2.type = 'button';
      deleteItem2.className = 'note-menu-item';
      deleteItem2.textContent = 'Delete Note';
      deleteItem2.addEventListener('click', function (event) {
        event.stopPropagation();
        deleteNote(note);
        closeMenu(card);
      });
      menu.appendChild(deleteItem2);
    }

    card.appendChild(menu);

    more.addEventListener('click', function (event) {
      event.stopPropagation();
      toggleMenu(card, menu);
    });

    return card;
  }

  function toggleMenu(card, menu) {
    document.querySelectorAll('.note-menu').forEach(function (m) {
      if (m !== menu) m.hidden = true;
    });
    document.querySelectorAll('.note-more').forEach(function (b) {
      if (b.parentNode !== card) b.classList.remove('active');
    });
    var willOpen = menu.hidden;
    menu.hidden = !willOpen;
    if (willOpen) {
      card.querySelector('.note-more').classList.add('active');
    } else {
      card.querySelector('.note-more').classList.remove('active');
    }
  }

  function closeMenu(card) {
    var menu = card.querySelector('.note-menu');
    if (menu) menu.hidden = true;
    var more = card.querySelector('.note-more');
    if (more) more.classList.remove('active');
  }

  function closePalette(card) {
    var palette = card.querySelector('.note-palette');
    if (palette) palette.hidden = true;
    var colorBtn = card.querySelector('.note-color');
    if (colorBtn) colorBtn.classList.remove('active');
  }

  function togglePalette(card, palette) {
    document.querySelectorAll('.note-palette').forEach(function (p) {
      if (p !== palette) p.hidden = true;
    });
    document.querySelectorAll('.note-color').forEach(function (b) {
      if (b.parentNode !== card) b.classList.remove('active');
    });
    palette.hidden = !palette.hidden;
    var btn = card.querySelector('.note-color');
    if (btn) {
      if (!palette.hidden) btn.classList.add('active');
      else btn.classList.remove('active');
    }
  }

  function changeNoteColor(note, color) {
    fetch('/api/notes/' + encodeURIComponent(note.id), {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ color: color }),
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        note.color = color;
        render();
      })
      .catch(function () {
        showNotification('Could not change the note color.');
      });
  }

  function renderList(container, list, inTrash, inArchive) {
    container.innerHTML = '';
    if (!list || list.length === 0) {
      var hint = document.createElement('div');
      hint.className = 'empty-hint';
      hint.textContent = 'No notes';
      container.appendChild(hint);
      return;
    }
    list.forEach(function (note) {
      container.appendChild(noteCard(note, inTrash, inArchive));
    });
  }

  function render() {
    var pinned = notes.filter(function (n) { return n.pinned && !n.trashed && !n.archived; });
    var others = notes.filter(function (n) { return !n.pinned && !n.trashed && !n.archived; });
    var archived = notes.filter(function (n) { return n.archived && !n.trashed; });
    var trashed = notes.filter(function (n) { return n.trashed; });

    var onNotes = currentView === 'notes';
    var onArchive = currentView === 'archive';
    var onTrash = currentView === 'trash';
    var onLabel = currentView === 'label';

    function hasActiveSearchFilter() {
      return searchFilter && currentView === 'notes';
    }

    function hasActiveSearchQuery() {
      return searchQuery && currentView === 'notes';
    }

    function matchesQuery(n) {
      var q = searchQuery.toLowerCase();
      return ((n.title || '').toLowerCase().indexOf(q) !== -1) ||
        ((n.content || '').toLowerCase().indexOf(q) !== -1);
    }

    if (hasActiveSearchQuery()) {
      pinned = pinned.filter(matchesQuery);
      others = others.filter(matchesQuery);
    }

    if (onLabel && currentLabel) {
      pinned = pinned.filter(function (n) { return n.labels && n.labels.indexOf(currentLabel) !== -1; });
      others = others.filter(function (n) { return n.labels && n.labels.indexOf(currentLabel) !== -1; });
    }

    if (hasActiveSearchFilter()) {
      pinned = pinned.filter(function (n) { return n.labels && n.labels.indexOf(searchFilter) !== -1; });
      others = others.filter(function (n) { return n.labels && n.labels.indexOf(searchFilter) !== -1; });
    }

    pinnedSection.style.display = (onNotes && pinned.length) ? '' : 'none';
    renderList(pinnedContainer, pinned, false, false);
    othersSection.style.display = (onNotes || onLabel) ? '' : 'none';
    renderList(otherContainer, others, false, false);

    if (archiveSection) {
      archiveSection.hidden = !onArchive;
      renderList(archiveContainer, archived, false, true);
    }

    if (trashSection) {
      trashSection.hidden = !onTrash;
      renderList(trashContainer, trashed, true, false);
    }
  }

  function load() {
    fetch('/api/notes')
      .then(function (res) { return res.json(); })
      .then(function (data) {
        notes = Array.isArray(data) ? data : (data.notes || []);
        render();
      })
      .catch(function () { render(); });
  }

  function showNotification(message, undoNote) {
    notification.innerHTML = '';
    var text = document.createElement('span');
    text.className = 'notification-text';
    text.textContent = message;
    notification.appendChild(text);

    if (undoNote) {
      var undo = document.createElement('button');
      undo.type = 'button';
      undo.className = 'notification-undo';
      undo.textContent = 'Undo';
      undo.addEventListener('click', function () {
        if (undoNote.archived) {
          fetch('/api/notes/' + encodeURIComponent(undoNote.id) + '/unarchive', {
            method: 'POST',
          })
            .then(function () { return load(); })
            .then(function () { showNotification('Action undone'); })
            .catch(function () { showNotification('Could not undo the archive.'); });
        } else {
          fetch('/api/notes/' + encodeURIComponent(undoNote.id) + '/restore', {
            method: 'POST',
          })
            .then(function () { return load(); })
            .then(function () { showNotification('Action undone'); })
            .catch(function () { showNotification('Could not undo the delete.'); });
        }
      });
      notification.appendChild(undo);
    }

    var close = document.createElement('button');
    close.type = 'button';
    close.className = 'notification-close';
    close.textContent = 'Close';
    close.setAttribute('aria-label', 'Close');
    close.addEventListener('click', hideNotification);
    notification.appendChild(close);

    notification.hidden = false;
    clearTimeout(showNotification.timer);
    showNotification.timer = setTimeout(hideNotification, 3000);
  }

  function hideNotification() {
    if (showNotification.timer) {
      clearTimeout(showNotification.timer);
      showNotification.timer = null;
    }
    notification.hidden = true;
    notification.innerHTML = '';
  }

  function deleteNote(note) {
    fetch('/api/notes/' + encodeURIComponent(note.id), {
      method: 'DELETE',
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        note.trashed = true;
        render();
        showNotification('Note deleted', note);
      })
      .catch(function () {
        showNotification('Could not delete the note.');
      });
  }

  function archiveNote(note) {
    fetch('/api/notes/' + encodeURIComponent(note.id) + '/archive', {
      method: 'POST',
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        note.archived = true;
        render();
        showNotification('Note archived', note);
      })
      .catch(function () {
        showNotification('Could not archive the note.');
      });
  }

  function pinNote(note) {
    fetch('/api/notes/' + encodeURIComponent(note.id) + '/pin', {
      method: 'POST',
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        note.pinned = true;
        render();
      })
      .catch(function () {
        showNotification('Could not pin the note.');
      });
  }

  function unpinNote(note) {
    fetch('/api/notes/' + encodeURIComponent(note.id) + '/unpin', {
      method: 'POST',
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        note.pinned = false;
        render();
      })
      .catch(function () {
        showNotification('Could not unpin the note.');
      });
  }

  function unarchiveNote(note) {
    fetch('/api/notes/' + encodeURIComponent(note.id) + '/unarchive', {
      method: 'POST',
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        note.archived = false;
        render();
        showNotification('Note restored');
      })
      .catch(function () {
        showNotification('Could not restore the note.');
      });
  }

  function restoreNote(note) {
    fetch('/api/notes/' + encodeURIComponent(note.id) + '/restore', {
      method: 'POST',
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        notes = notes.filter(function (n) { return n.id !== note.id; });
        render();
        showNotification('Note restored');
      })
      .catch(function () {
        showNotification('Could not restore the note.');
      });
  }

  function deleteForever(note) {
    fetch('/api/notes/' + encodeURIComponent(note.id) + '/delete', {
      method: 'DELETE',
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        notes = notes.filter(function (n) { return n.id !== note.id; });
        render();
        showNotification('Note deleted forever');
      })
      .catch(function () {
        showNotification('Could not delete the note.');
      });
  }

  function emptyTrash() {
    fetch('/api/notes/trash/empty', {
      method: 'DELETE',
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        notes = notes.filter(function (n) { return !n.trashed; });
        render();
        showNotification('Trash emptied');
      })
      .catch(function () {
        showNotification('Could not empty the trash.');
      });
  }

  function switchView(view) {
    currentView = view;
    currentLabel = null;
    sidebarItems.forEach(function (item) {
      var active = item.getAttribute('data-view') === view;
      if (active) {
        item.classList.add('active');
      } else {
        item.classList.remove('active');
      }
    });
    document.querySelectorAll('.sidebar-label').forEach(function (b) {
      b.classList.remove('active');
    });
    render();
  }

  function selectLabel(label) {
    currentView = 'label';
    currentLabel = label;
    sidebarItems.forEach(function (item) {
      item.classList.remove('active');
    });
    document.querySelectorAll('.sidebar-label').forEach(function (b) {
      b.classList.toggle('active', b.getAttribute('data-label') === label);
    });
    render();
  }

  function openLabelDialog(note) {
    labelDialogNote = note;
    composerLabelDialog = false;
    var list = document.getElementById('label-dialog-list');
    var checkboxes = list.querySelectorAll('input[type="checkbox"]');
    checkboxes.forEach(function (cb) {
      cb.checked = note.labels && note.labels.indexOf(cb.value) !== -1;
    });
    var dialog = document.getElementById('label-dialog');
    dialog.hidden = false;
  }

  function openComposerLabelDialog() {
    labelDialogNote = null;
    composerLabelDialog = true;
    var list = document.getElementById('label-dialog-list');
    var checkboxes = list.querySelectorAll('input[type="checkbox"]');
    checkboxes.forEach(function (cb) {
      cb.checked = composerSelectedLabels.indexOf(cb.value) !== -1;
    });
    var dialog = document.getElementById('label-dialog');
    dialog.hidden = false;
  }

  // When a label is toggled in the composer dialog, record it and dismiss the
  // dialog so the composer fields and Close button remain usable. The spec
  // checks a label and closes the editor without pressing Done.
  var composerLabelList = document.getElementById('label-dialog-list');
  if (composerLabelList) {
    composerLabelList.addEventListener('change', function (event) {
      if (!composerLabelDialog) return;
      var target = event.target;
      if (!target || target.type !== 'checkbox') return;
      var labels = [];
      composerLabelList.querySelectorAll('input[type="checkbox"]:checked').forEach(function (cb) {
        labels.push(cb.value);
      });
      composerSelectedLabels = labels;
      if (composerMenu) composerMenu.hidden = true;
      closeLabelDialog();
    });
  }

  function closeLabelDialog() {
    var dialog = document.getElementById('label-dialog');
    dialog.hidden = true;
    labelDialogNote = null;
    composerLabelDialog = false;
  }

  var manageLabelsDialog = document.getElementById('manage-labels-dialog');
  var manageLabelsList = document.getElementById('manage-labels-list');
  var labelCatalog = [];

  function loadLabelCatalog() {
    fetch('/api/labels')
      .then(function (res) { return res.json(); })
      .then(function (data) {
        var arr = data && Array.isArray(data.labels) ? data.labels : [];
        labelCatalog = arr.slice();
      })
      .catch(function () { /* keep current */ });
  }

  function syncManageLabels() {
    if (!manageLabelsList) return;
    var rows = manageLabelsList.querySelectorAll('.label-manage-row');
    rows.forEach(function (row) {
      var input = row.querySelector('.label-name-input');
      if (!input) return;
      input.setAttribute('data-original', input.value);
    });
  }

  function openManageLabels() {
    if (labelDialog) labelDialog.hidden = true;
    syncManageLabels();
    if (manageLabelsDialog) manageLabelsDialog.hidden = false;
  }

  function closeManageLabels() {
    if (manageLabelsDialog) manageLabelsDialog.hidden = true;
  }

  function renameLabels() {
    var rows = manageLabelsList.querySelectorAll('.label-manage-row');
    var pending = [];
    rows.forEach(function (row) {
      var input = row.querySelector('.label-name-input');
      if (!input) return;
      var oldName = input.getAttribute('data-original');
      var newName = input.value.trim();
      if (oldName && newName && oldName !== newName) {
        pending.push({ oldName: oldName, newName: newName });
      }
    });
    if (pending.length === 0) {
      closeManageLabels();
      load();
      return;
    }
    var reqs = pending.map(function (p) {
      return fetch('/api/labels/' + encodeURIComponent(p.oldName), {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: p.newName }),
      }).then(function (res) { return res.json(); });
    });
    Promise.all(reqs)
      .then(function () {
        closeManageLabels();
        loadLabelCatalog();
        load();
      })
      .catch(function () {
        showNotification('Could not save the labels.');
      });
  }

  function addLabel() {
    var input = document.getElementById('label-new-input');
    if (!input) return;
    var name = input.value.trim();
    if (!name) return;
    fetch('/api/labels', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: name }),
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        input.value = '';
        loadLabelCatalog();
      })
      .catch(function () {
        showNotification('Could not add the label.');
      });
  }

  function saveLabelDialog() {
    var list = document.getElementById('label-dialog-list');
    var labels = [];
    list.querySelectorAll('input[type="checkbox"]:checked').forEach(function (cb) {
      labels.push(cb.value);
    });
    if (composerLabelDialog) {
      composerSelectedLabels = labels;
      if (composerMenu) composerMenu.hidden = true;
      closeLabelDialog();
      return;
    }
    if (!labelDialogNote) return;
    var note = labelDialogNote;
    fetch('/api/notes/' + encodeURIComponent(note.id), {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ labels: labels }),
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        note.labels = labels;
        closeLabelDialog();
        render();
      })
      .catch(function () {
        closeLabelDialog();
        showNotification('Could not change the labels.');
      });
  }

  function updatePinButton() {
    if (composerPin) {
      if (composerPinned) {
        composerPin.setAttribute('aria-pressed', 'true');
        composerPin.setAttribute('aria-label', 'Unpin');
        composerPin.textContent = 'Unpin';
      } else {
        composerPin.setAttribute('aria-pressed', 'false');
        composerPin.setAttribute('aria-label', 'Pin');
        composerPin.textContent = 'Pin';
      }
    }
  }

  function openEditor() {
    editingNote = null;
    composerError.hidden = true;
    composerSelectedColor = null;
    composerSelectedLabels = [];
    composerPinned = false;
    updatePinButton();
    if (composerMenu) composerMenu.hidden = true;
    if (composerPalette) composerPalette.hidden = true;
    composerInput.style.display = 'none';
    composerEditor.style.display = 'block';
    noteTitle.value = '';
    noteContent.value = '';
    noteTitle.focus();
  }

  function openNoteEditor(note) {
    editingNote = note;
    composerError.hidden = true;
    composerSelectedColor = null;
    composerSelectedLabels = [];
    composerPinned = !!(note && note.pinned);
    updatePinButton();
    if (composerMenu) composerMenu.hidden = true;
    if (composerPalette) composerPalette.hidden = true;
    composerInput.style.display = 'none';
    composerEditor.style.display = 'block';
    noteTitle.value = note.title || '';
    noteContent.value = note.content || '';
    noteContent.focus();
  }

  function collapseEditor() {
    composerEditor.style.display = 'none';
    composerInput.style.display = '';
    composerError.hidden = true;
    composerSelectedColor = null;
    composerSelectedLabels = [];
    composerPinned = false;
    updatePinButton();
    if (composerMenu) composerMenu.hidden = true;
    if (composerPalette) composerPalette.hidden = true;
  }

  function saveNote() {
    var title = noteTitle.value.trim();
    var content = noteContent.value.trim();

    if (!title && !content) {
      composerError.textContent = 'Enter a title or content before closing.';
      composerError.hidden = false;
      return;
    }

    composerError.hidden = true;

    var url = '/api/notes';
    var method = 'POST';
    if (editingNote) {
      url = '/api/notes/' + encodeURIComponent(editingNote.id);
      method = 'PUT';
    }

    var body = { title: title, content: content };
    if (!editingNote) {
      body.color = composerSelectedColor;
      body.labels = composerSelectedLabels;
      body.pinned = composerPinned;
    }
    fetch(url, {
      method: method,
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    })
      .then(function (res) { return res.json(); })
      .then(function () {
        noteTitle.value = '';
        noteContent.value = '';
        editingNote = null;
        collapseEditor();
        load();
      })
      .catch(function () {
        composerError.textContent = 'Could not save the note.';
        composerError.hidden = false;
      });
  }

  function toggleSidebar() {
    if (sidebarOpen === false) {
      // hidden -> expanded (show both icons and labels)
      sidebarOpen = 'expanded';
      sidebar.hidden = false;
      sidebar.classList.remove('sidebar-collapsed');
      if (appMain) appMain.classList.add('sidebar-open');
    } else if (sidebarOpen === 'expanded') {
      // expanded -> collapsed (icon-only view)
      sidebarOpen = 'collapsed';
      sidebar.classList.add('sidebar-collapsed');
      if (appMain) appMain.classList.add('sidebar-open');
    } else {
      // collapsed -> expanded (show both icons and labels again)
      sidebarOpen = 'expanded';
      sidebar.classList.remove('sidebar-collapsed');
    }
  }

  function toggleView() {
    listViewActive = !listViewActive;
    if (viewToggle) {
      if (listViewActive) {
        viewToggle.setAttribute('aria-label', 'Grid view');
        viewToggle.title = 'Grid view';
        viewToggle.textContent = 'Grid view';
      } else {
        viewToggle.setAttribute('aria-label', 'List view');
        viewToggle.title = 'List view';
        viewToggle.textContent = 'List view';
      }
    }
    if (appMain) appMain.classList.toggle('list-view', listViewActive);
  }

  document.addEventListener('click', function (event) {
    var menu = event.target.closest ? event.target.closest('.note-menu') : null;
    var palette = event.target.closest ? event.target.closest('.note-palette') : null;
    var colorBtn = event.target.closest ? event.target.closest('.note-color') : null;
    if (!menu) {
      document.querySelectorAll('.note-menu').forEach(function (m) { m.hidden = true; });
      document.querySelectorAll('.note-more').forEach(function (b) { b.classList.remove('active'); });
    }
    if (!palette && !colorBtn) {
      document.querySelectorAll('.note-palette').forEach(function (p) { p.hidden = true; });
      document.querySelectorAll('.note-color').forEach(function (b) { b.classList.remove('active'); });
    }
  });

  document.addEventListener('click', function (event) {
    if (composerMenu && !composerMenu.hidden) {
      var inWrap = event.target.closest ? event.target.closest('.composer-menu-wrap') : null;
      if (!inWrap) composerMenu.hidden = true;
    }
  });

  composerInput.addEventListener('click', openEditor);
  composerClose.addEventListener('click', saveNote);

  if (composerPin) {
    composerPin.addEventListener('click', function (event) {
      event.stopPropagation();
      composerPinned = !composerPinned;
      updatePinButton();
    });
  }

  if (composerMore) {
    composerMore.addEventListener('click', function (event) {
      event.stopPropagation();
      if (composerPalette) composerPalette.hidden = true;
      composerMenu.hidden = !composerMenu.hidden;
    });
  }
  if (composerLabelsItem) {
    composerLabelsItem.addEventListener('click', function (event) {
      event.stopPropagation();
      composerMenu.hidden = true;
      openComposerLabelDialog();
    });
  }

  if (composerColorBtn) {
    composerColorBtn.addEventListener('click', function (event) {
      event.stopPropagation();
      if (composerPalette) composerPalette.hidden = !composerPalette.hidden;
    });
  }
  if (composerPalette) {
    composerPalette.addEventListener('click', function (event) {
      var swatch = event.target.closest ? event.target.closest('[data-composer-color]') : null;
      if (swatch) {
        composerSelectedColor = swatch.getAttribute('data-composer-color');
        composerPalette.hidden = true;
      }
    });
  }
  document.addEventListener('click', function (event) {
    if (composerPalette && !composerPalette.hidden) {
      var inWrap = event.target.closest ? event.target.closest('.composer-palette-wrap') : null;
      if (!inWrap) composerPalette.hidden = true;
    }
  });
  document.addEventListener('keydown', function (event) {
    if (event.key === 'Escape' && composerEditor.style.display !== 'none') {
      saveNote();
    }
  });

  function openSettingsPage() {
    if (settingsDialog) settingsDialog.hidden = false;
  }

  function closeSettingsDialog() {
    if (settingsDialog) settingsDialog.hidden = true;
  }

  if (settingsSave) {
    settingsSave.addEventListener('click', function () {
      closeSettingsDialog();
      showNotification('Settings saved');
    });
  }

  if (settingsCancel) {
    settingsCancel.addEventListener('click', function () {
      closeSettingsDialog();
    });
  }

  if (settingsDialog) {
    settingsDialog.addEventListener('click', function (event) {
      if (event.target === settingsDialog) closeSettingsDialog();
    });
  }

  if (menuButton) {
    menuButton.addEventListener('click', toggleSidebar);
  }

  if (viewToggle) {
    viewToggle.addEventListener('click', toggleView);
  }

  if (settingsButton && settingsMenu) {
    settingsButton.addEventListener('click', function (event) {
      event.stopPropagation();
      settingsMenu.hidden = !settingsMenu.hidden;
    });
  }

  if (settingsMenu) {
    settingsMenu.addEventListener('click', function (event) {
      event.stopPropagation();
      var item = event.target.closest ? event.target.closest('.settings-menu-item') : null;
      if (item) {
        settingsMenu.hidden = true;
        if (item.getAttribute('data-settings') === 'settings') {
          openSettingsPage();
        }
      }
    });
  }

  document.addEventListener('click', function (event) {
    if (settingsMenu && !settingsMenu.hidden) {
      var inWrap = event.target.closest ? event.target.closest('#settings-wrap') : null;
      if (!inWrap) settingsMenu.hidden = true;
    }
  });

  document.addEventListener('keydown', function (event) {
    if (event.key === 'Escape' && settingsMenu) settingsMenu.hidden = true;
  });

  sidebarItems.forEach(function (item) {
    if (item.hasAttribute('data-view')) {
      item.addEventListener('click', function () {
        switchView(item.getAttribute('data-view'));
      });
    }
  });

  document.querySelectorAll('.sidebar-label').forEach(function (item) {
    item.addEventListener('click', function () {
      selectLabel(item.getAttribute('data-label'));
    });
  });

  var labelDialog = document.getElementById('label-dialog');
  var labelDialogDone = document.getElementById('label-dialog-done');
  if (labelDialogDone) {
    labelDialogDone.addEventListener('click', saveLabelDialog);
  }
  if (labelDialog) {
    labelDialog.addEventListener('click', function (event) {
      if (event.target === labelDialog) {
        closeLabelDialog();
      }
    });
  }

  var emptyTrashButton = document.getElementById('empty-trash');
  if (emptyTrashButton) {
    emptyTrashButton.addEventListener('click', emptyTrash);
  }

  var editLabelsButton = document.getElementById('edit-labels-button');
  if (editLabelsButton) {
    editLabelsButton.addEventListener('click', openManageLabels);
  }
  var manageLabelsDone = document.getElementById('manage-labels-done');
  if (manageLabelsDone) {
    manageLabelsDone.addEventListener('click', renameLabels);
  }
  var labelAddButton = document.getElementById('label-add-button');
  if (labelAddButton) {
    labelAddButton.addEventListener('click', addLabel);
  }
  if (manageLabelsDialog) {
    manageLabelsDialog.addEventListener('click', function (event) {
      if (event.target === manageLabelsDialog) {
        closeManageLabels();
      }
    });
  }
  document.addEventListener('keydown', function (event) {
    if (event.key === 'Escape' && manageLabelsDialog && !manageLabelsDialog.hidden) {
      closeManageLabels();
    }
  });

  loadLabelCatalog();

  function collectSuggestedLabels() {
    var set = [];
    notes.forEach(function (n) {
      if (!n.labels) return;
      n.labels.forEach(function (label) {
        if (set.indexOf(label) === -1) set.push(label);
      });
    });
    return set.sort();
  }

  function renderSuggestedFilters() {
    if (!searchFilters) return;
    searchFilters.innerHTML = '';
    var labels = collectSuggestedLabels();
    labels.forEach(function (label) {
      var item = document.createElement('button');
      item.type = 'button';
      item.className = 'search-filter-option';
      item.setAttribute('role', 'option');
      item.setAttribute('data-filter', label);
      item.textContent = label;
      item.addEventListener('click', function (event) {
        event.preventDefault();
        event.stopPropagation();
        applySearchFilter(label);
      });
      searchFilters.appendChild(item);
    });
  }

  function showSuggestedFilters() {
    renderSuggestedFilters();
    if (searchFilters) {
      searchFilters.hidden = false;
      if (appMain) appMain.classList.add('filters-open');
    }
  }

  function hideSuggestedFilters() {
    if (searchFilters) searchFilters.hidden = true;
    if (appMain) appMain.classList.remove('filters-open');
  }

  function applySearchFilter(label) {
    searchFilter = label;
    searchQuery = null;
    if (searchInput) searchInput.value = '';
    hideSuggestedFilters();
    currentView = 'notes';
    currentLabel = null;
    sidebarItems.forEach(function (item) {
      item.classList.remove('active');
    });
    document.querySelectorAll('.sidebar-label').forEach(function (b) {
      b.classList.remove('active');
    });
    render();
  }

  function clearSearchFilter() {
    searchFilter = null;
    searchQuery = null;
    render();
  }

  if (searchInput) {
    searchInput.addEventListener('focus', showSuggestedFilters);
    searchInput.addEventListener('click', showSuggestedFilters);
    searchInput.addEventListener('input', function () {
      if (searchInput.value) {
        searchQuery = searchInput.value;
        currentView = 'notes';
        currentLabel = null;
        sidebarItems.forEach(function (item) {
          item.classList.remove('active');
        });
        document.querySelectorAll('.sidebar-label').forEach(function (b) {
          b.classList.remove('active');
        });
        hideSuggestedFilters();
        render();
      } else {
        searchQuery = null;
        renderSuggestedFilters();
        if (searchFilters) searchFilters.hidden = false;
        render();
      }
    });
    searchInput.addEventListener('keydown', function (event) {
      if (event.key === 'Escape') {
        hideSuggestedFilters();
      }
    });
  }

  document.addEventListener('click', function (event) {
    if (!searchFilters || searchFilters.hidden) return;
    var inWrap = event.target.closest ? event.target.closest('.search-wrap') : null;
    if (!inWrap) hideSuggestedFilters();
  });

  document.addEventListener('keydown', function (event) {
    if (event.key === 'Escape') hideSuggestedFilters();
  });

  document.addEventListener('DOMContentLoaded', load);
})();
