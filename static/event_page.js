document.getElementById("button-delete")?.addEventListener("click", (e) => {
  e.preventDefault();
  const eventId = document
    .getElementById("button-delete")
    .getAttribute("event-id");
  const result = window.confirm("Are you sure you want to delete this event?");
  if (result) {
    fetch(`/events/${eventId}`, { method: "DELETE" })
      .then((response) => {
        if (response.status === 200) {
          window.location = "/events";
        } else {
          console.error(response);
          window.alert(`Something went wrong; got status ${response.status}`);
        }
      })
      .catch((error) => {
        console.error(error);
        window.alert(`Something went wrong: ${error}`);
      });
  }
});

document.querySelectorAll(".btn-position-add").forEach((button) => {
  button.addEventListener("click", () => {
    document.getElementById("new-position-category").value =
      button.getAttribute("category");
    document.getElementById("modalAddPosition").showModal();
  });
});

document.querySelectorAll(".btn-position-set").forEach((button) => {
  button.addEventListener("click", () => {
    document.getElementById("set-position-id").value =
      button.getAttribute("position_id");
    document.getElementById("modalSetPosition").showModal();
  });
});

// can't nest forms in HTML
document
  .getElementById("btn-modal-register-unregister")
  ?.addEventListener("click", (e) => {
    e.preventDefault();
    const eventId = e.target.getAttribute("event-id");
    const result = window.confirm(
      "Are you sure you want to remove yourself from this event?"
    );
    if (result) {
      fetch(`/events/${eventId}/unregister`, { method: "POST" })
        .then((response) => {
          window.location.reload();
        })
        .catch((error) => {
          console.error(error);
          window.alert(`Something went wrong: ${error}`);
        });
    }
  });

// have to do it this way so the forms don't submit
document
  .getElementById("btn-modal-edit-close")
  .addEventListener("click", (e) => {
    e.preventDefault();
    document.getElementById("modalEditForm").close();
  });

document
  .getElementById("btn-modal-register-close")
  .addEventListener("click", (e) => {
    e.preventDefault();
    document.getElementById("modalRegisterForm").close();
  });

document
  .getElementById("btn-modal-add-position-close")
  .addEventListener("click", (e) => {
    e.preventDefault();
    document.getElementById("modalAddPosition").close();
    document.getElementById("new-position-category").value = "";
  });

document
  .getElementById("btn-modal-set-position-close")
  .addEventListener("click", (e) => {
    e.preventDefault();
    document.getElementById("modalSetPosition").close();
    document.getElementById("set-position-id").value = "";
  });

document
  .getElementById("btn-modal-no-show-close")
  .addEventListener("click", (e) => {
    e.preventDefault();
    document.getElementById("modalNoShow").close();
  });

document
  .getElementById("modalAddPosition")
  .querySelectorAll('input[type="text"]')
  .forEach((input) => {
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        document
          .getElementById("modalAddPosition")
          .querySelector("form")
          .submit();
      }
    });
  });
