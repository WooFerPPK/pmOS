
export class TerminalUI {
    constructor(windowElement) {
        this.windowElement = windowElement;
        this.createUI();
        this.terminalElement = this.windowElement.querySelector('.terminal-container');
    }

    createUI() {
        const terminalContainer = document.createElement('div');
        terminalContainer.className = 'terminal-container';

        const outputDiv = document.createElement('div');
        outputDiv.id = "output";
        outputDiv.className = "output";
        terminalContainer.appendChild(outputDiv);
    
        // Create input container div
        const inputContainer = document.createElement('div');
        inputContainer.className = "input-container";
    
        const span = document.createElement('span');
        span.innerHTML = ">>&nbsp;";
        inputContainer.appendChild(span);  // Append span to input container
    
        const inputDiv = document.createElement('div');
        inputDiv.id = "input";
        inputDiv.className = "input";
        inputDiv.contentEditable = "true";
        inputContainer.appendChild(inputDiv);  // Append inputDiv to input container
    
        // Append the input container to terminalElement
        terminalContainer.appendChild(inputContainer);
        this.windowElement.appendChild(terminalContainer);
    }    

    destroyTerminal() {
        this.windowElement.remove();
    }

    initFocusHandler(inputHandler) {
        // Store the listener function in clickHandler
        this.clickHandler = () => {
            if (inputHandler && inputHandler.inputElement) {
                inputHandler.inputElement.focus();
                inputHandler.inputElement.scrollIntoView({
                    behavior: 'smooth',
                    block: 'center'
                });
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
            this.clickHandler = null;  // Clear the stored handler
        }
    }
}
