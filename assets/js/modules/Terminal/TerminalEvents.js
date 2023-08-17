
export class TerminalEvents {
    constructor() {
        this.eventListeners = {};
        this.boundHandleCtrlC = null;
    }

    on(event, listener) {
        if (!this.eventListeners[event]) {
            this.eventListeners[event] = [];
        }
        this.eventListeners[event].push(listener);
    }

    off(event, listener) {
        if (!this.eventListeners[event]) return;
        const index = this.eventListeners[event].indexOf(listener);
        if (index !== -1) {
            this.eventListeners[event].splice(index, 1);
        }
    }

    emit(event, data) {
        if (!this.eventListeners[event]) return;
        for (let listener of this.eventListeners[event]) {
            listener(data);
        }
    }

    initCtrlCListener(outputHandler) {
        this.boundHandleCtrlC = this.handleCtrlC.bind(this, outputHandler);
        document.addEventListener('keydown', this.boundHandleCtrlC);
    }

    removeCtrlCListener() {
        if (this.boundHandleCtrlC) {
            document.removeEventListener('keydown', this.boundHandleCtrlC);
            this.boundHandleCtrlC = null;
        }
    }

    handleCtrlC(outputHandler, event) {
        if (this.isCtrlCPressed(event)) {
            event.preventDefault();
            outputHandler.interruptOutput();
        }
    }

    isCtrlCPressed(event) {
        return event.ctrlKey && event.key === 'c';
    }
    
}
