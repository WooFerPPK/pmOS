/**
 * TemplateLoader - A class responsible for loading templates asynchronously and injecting them into a container.
 */
export default class TemplateLoader {
    /**
     * Creates a new TemplateLoader.
     *
     * @param {HTMLElement} container - The container where the template will be appended.
     * @param {any} observable - An observable object (not used in the provided code but presumably for future features).
     * @param {string} templateUrl - URL from which the HTML template will be fetched.
     * @param {string} templateClass - A CSS class to wrap around the loaded template content.
     * @param {Function} callback - Callback function to be invoked after the template is appended to the container.
     */
    constructor(container, observable, templateUrl, templateClass, callback) {
        this.container = container;
        this.observable = observable;
        this.templateHTMLContent = null;
        this.templateContentWrapper = null;
        this.templateUrl = templateUrl;
        this.templateClass = templateClass;
        this.callback = callback;
        this.templateHTMLFetchPromise = this.fetchHTML(); // Fetch the HTML content upon instantiation.
    }

    /**
     * Fetches the HTML content from the provided URL.
     *
     * @async
     * @private
     * @return {Promise<void>}
     */
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

    /**
     * Opens (appends) the fetched template into the container.
     * If the template hasn't been fetched yet, it waits for it.
     *
     * @async
     * @return {Promise<void>}
     */
    async open() {
        // Wait for the template if it hasn't been fetched yet.
        if (!this.templateHTMLContent) {
            await this.templateHTMLFetchPromise;
        }

        // If the template is ready, process and append it.
        if (this.templateHTMLContent) {
            const parser = new DOMParser();
            const doc = parser.parseFromString(`<div class="${this.templateClass} container">${this.templateHTMLContent}</div>`, 'text/html');
            this.templateContentWrapper = doc.querySelector(`.${this.templateClass}`);

            this.container.appendChild(this.templateContentWrapper);

            // Invoke the callback if it's a function, passing the appended content as an argument.
            if (typeof this.callback === 'function') {
                this.callback(this.templateContentWrapper);
            }
        } else {
            console.error('Template not fetched yet');
        }
    }

    /**
     * Closes (removes) the template from the container.
     */
    close() {
        if (this.templateContentWrapper) {
            this.templateContentWrapper.remove();
            this.templateContentWrapper = null;
        }
    }
}
