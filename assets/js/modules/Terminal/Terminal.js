
import { InputHandler } from './InputHandler.js';
import { OutputHandler } from './OutputHandler.js';
import { TerminalUI } from './TerminalUI.js';
import { TerminalEvents } from './TerminalEvents.js';
import { TerminalState } from './TerminalState.js';

export class Terminal {
    constructor(windowElement, observable, interactiveWindows) {
        this.observable = observable;
        this.interactiveWindows = interactiveWindows;
        
        this.terminalUI = new TerminalUI(windowElement);
        this.terminalEvents = new TerminalEvents(this);
        this.terminalState = new TerminalState();

        this.outputHandler = new OutputHandler(windowElement.querySelector('#output'), this);
        this.inputHandler = new InputHandler(windowElement.querySelector('#input'), this);
        this.inputHandler.initInputListener();
        this.outputHandler.displayStartupMessage();
        this.terminalUI.initFocusHandler(this.inputHandler);
        this.terminalEvents.initCtrlCListener(this.outputHandler, this.terminalElement);
    }
    
    get isOutputRunning() {
        return this.terminalState.isOutputRunning;
    }

    set isOutputRunning(value) {
        this.terminalState.isOutputRunning = value;
        this.terminalEvents.emit('outputRunningChanged', value);
    }

    addCommandToHistory(command) {
        this.terminalState.addCommandToHistory(command);
    }

    lastCommand() {
        return this.terminalState.lastCommand();
    }
}
