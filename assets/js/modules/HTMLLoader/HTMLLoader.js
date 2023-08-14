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
    
                // Generate a unique ID for the instance (not specific to calculator)
                const uniqueId = `instance-${Date.now()}-${Math.random().toString(36).substr(2, 9)}`;
    
                // Create a div with the required class
                const htmlContainer = document.createElement('div');
                htmlContainer.className = "htmlviewer container";
                htmlContainer.id = uniqueId;  // Set the unique ID
        
                // Create a shadow root for the container
                const shadowRoot = htmlContainer.attachShadow({ mode: 'open' });
                shadowRoot.innerHTML = data;
    
                // Modify the script to access the shadowRoot directly using the unique ID
                const shadowRootReference = `document.getElementById("${uniqueId}").shadowRoot`;
                data = data.replace(/__SHADOWROOT_REFERENCE__/g, shadowRootReference);
        
                // Execute scripts
                const scripts = shadowRoot.querySelectorAll("script");
                scripts.forEach((script) => {
                    const newScript = document.createElement("script");
                    if (script.type) {
                        newScript.type = script.type; // Preserve the type attribute
                    }
                    if (script.src) {
                        newScript.src = script.src;
                    } else {
                        newScript.textContent = script.textContent.replace(/__SHADOWROOT_REFERENCE__/g, shadowRootReference);
                    }
                    shadowRoot.appendChild(newScript);
                    script.remove();
                });
                    
                // Append the div to the main container
                this.container.appendChild(htmlContainer);
            } else {
                this.displayError('Failed to fetch the HTML content.');
            }
        } catch (e) {
            this.displayError(`There was an error loading the HTML content: ${e.message}`);
        }
    }
    
    displayError(message) {
        const errorMessage = document.createElement('p');
        errorMessage.className = 'error-message';
        errorMessage.textContent = message;
        this.container.appendChild(errorMessage);
    }
}
