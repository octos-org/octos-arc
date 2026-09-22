// BookStack frontend application bootstrap.
// The app is a plain HTML/CSS/JS application served from the backend.
// API interaction (if any) goes through the /api/ endpoints on the same origin.
(function () {
  'use strict';

  document.addEventListener('DOMContentLoaded', function () {
    const brand = document.querySelector('.brand');
    if (brand) {
      brand.addEventListener('click', function () {
        window.location.href = '/';
      });
    }
  });
})();
