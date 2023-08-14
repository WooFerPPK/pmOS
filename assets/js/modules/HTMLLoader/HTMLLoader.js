export default class HTMLLoader {
    constructor(container, htmlPath, observable) {
        this.observable = observable;

        this.observable.subscribe("windowShutdown", this);

        this.container = container;
        this.htmlPath = htmlPath;
    }

    update(message) {
        if (message === "shutdown") {
            this.container.remove();
        }
    }

    async load() {
        try {
            let response = await fetch(this.htmlPath);
            if (response.ok) {
                let data = await response.text();

                // Create a div with the required class
                const htmlContainer = document.createElement('div');
                htmlContainer.className = "htmlviewer container";

                // Create a shadow root for the container
                const shadowRoot = htmlContainer.attachShadow({ mode: 'open' });
                shadowRoot.innerHTML = data;

                // Append the div to the main container
                this.container.appendChild(htmlContainer);
            } else {
                this.displayError('Failed to fetch the HTML version of the resume.');
            }
        } catch (e) {
            this.displayError(`There was an error loading the HTML version: ${e.message}`);
        }
    }

    displayError(message) {
        const errorMessage = document.createElement('p');
        errorMessage.className = 'error-message';
        errorMessage.textContent = message;
        this.container.appendChild(errorMessage);
    }
}
