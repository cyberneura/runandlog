// The Third-Party Licenses window: shows the text the binary carries.
//
// Set with textContent, never innerHTML: the notices are text copied out of the
// libraries' own files, and are no more to be run as markup than a command's
// output is.
"use strict"

const target = document.getElementById("licenses-text")

window.__TAURI__.core
  .invoke("third_party_notices")
  .then((text) => {
    target.textContent = text
  })
  .catch((error) => {
    target.textContent = `Could not load the licenses: ${error}`
  })
