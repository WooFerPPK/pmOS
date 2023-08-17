
export class TerminalUI {
    constructor(windowElement, observable) {
        this.observable = observable;
        this.windowElement = windowElement;
        this.createUI();
        this.terminalElement = this.windowElement.querySelector('.terminal');
    }

    createUI() {
        const terminalContainer = document.createElement('div');
        terminalContainer.className = 'terminal container';

        const outputDiv = document.createElement('div');
        outputDiv.id = 'output';
        outputDiv.className = 'output';
        terminalContainer.appendChild(outputDiv);
    
        const inputContainer = document.createElement('div');
        inputContainer.className = 'input-container';
    
        const span = document.createElement('span');
        span.innerHTML = '>>&nbsp;';
        inputContainer.appendChild(span);
    
        const inputDiv = document.createElement('div');
        inputDiv.id = 'input';
        inputDiv.className = 'input';
        inputDiv.contentEditable = 'true';
        inputContainer.appendChild(inputDiv);
    
        terminalContainer.appendChild(inputContainer);
        this.windowElement.appendChild(terminalContainer);
    }    

    destroyTerminal() {
        this.observable.notify('windowClosed', { source: 'terminalUI', message: this.windowElement });
    }

    initFocusHandler(inputHandler) {
        // Store the listener function in clickHandler
        this.clickHandler = () => {
            if (inputHandler && inputHandler.inputElement) {
                inputHandler.inputElement.focus();
            } else {
                // If inputHandler doesn't exist anymore, remove the event listener
                this.removeFocusHandler();
            }
        };

        this.terminalElement.addEventListener('click', this.clickHandler);
    }

    // Method to remove the event listener
    removeFocusHandler() {
        if (this.clickHandler) {
            this.terminalElement.removeEventListener('click', this.clickHandler);
            this.clickHandler = null; // Clear the stored handler
        }
    }
}
