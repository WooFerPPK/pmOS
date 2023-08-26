/**
 * Manages the User Interface of the Terminal, including the creation 
 * of the terminal display and handling user interactions.
  * @class
 */
export class TerminalUI {
    /**
     * Creates a new TerminalUI instance.
     *
     * @param {HTMLElement} windowElement - The parent element to append the terminal UI to.
     * @param {Observable} observable - The observable instance used for event notifications.
     */
    constructor(windowElement, observable) {
        this.observable = observable;
        this.windowElement = windowElement;

        // Build the initial terminal UI
        this.createUI();

        // Query the terminal element for further use
        this.terminalElement = this.windowElement.querySelector('.terminal');
    }

    /**
     * Generates and appends the terminal UI components to the windowElement.
     */
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

    /**
     * Notifies observers that the terminal window is closing.
     */
    destroyTerminal() {
        this.observable.notify('windowClosed', { source: 'terminalUI', message: this.windowElement });
    }

    /**
     * Initializes a click handler on the terminal that sets focus to the input.
     * @param {InputHandler} inputHandler - The input handler associated with the terminal input.
     */
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

    /**
     * Removes the focus handler from the terminal.
     */
    removeFocusHandler() {
        if (this.clickHandler) {
            this.terminalElement.removeEventListener('click', this.clickHandler);
            this.clickHandler = null; // Clear the stored handler
        }
    }
}
