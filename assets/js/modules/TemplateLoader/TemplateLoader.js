export default class TemplateLoader {
    constructor(container, observable, templateUrl, templateClass, callback) {
        this.container = container;
        this.observable = observable;
        this.templateHTMLContent = null;
        this.templateContentWrapper = null;
        this.templateUrl = templateUrl;
        this.templateClass = templateClass;
        this.callback = callback;
        this.templateHTMLFetchPromise = this.fetchHTML();
    }

    async fetchHTML() {
        try {
            const response = await fetch(this.templateUrl);
            if (!response.ok) {
                throw new Error(`Could not get template from ${this.templateUrl}`);
            }

            const html = await response.text();
            this.templateHTMLContent = html;

        } catch (error) {
            console.error(error);
        }
    }

    async open() {
        if (!this.templateHTMLContent) {
            await this.templateHTMLFetchPromise;
        }

        if (this.templateHTMLContent) {
            const parser = new DOMParser();
            const doc = parser.parseFromString(`<div class="${this.templateClass} container">${this.templateHTMLContent}</div>`, 'text/html');
            this.templateContentWrapper = doc.querySelector(`.${this.templateClass}`);

            this.container.appendChild(this.templateContentWrapper);

            // Invoke the callback with the appended template content as an argument
            if (typeof this.callback === 'function') {
                this.callback(this.templateContentWrapper);
            }
        } else {
            console.error("Template not fetched yet");
        }
    }

    close() {
        if (this.templateContentWrapper) {
            this.templateContentWrapper.remove();
            this.templateContentWrapper = null;
        }
    }
}

/**
 const loader = new TemplateLoader(
    containerElement, 
    observableObject, 
    "/path/to/your/template.html", 
    "customTemplateClass",
    (templateElement) => {
        // Custom logic or event listeners for the loaded template
        templateElement.addEventListener('click', (e) => {
            console.log('Template clicked!');
        });
    }
);

loader.open();

 */