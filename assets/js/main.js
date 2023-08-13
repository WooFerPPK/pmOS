import { Terminal } from './modules/Terminal/Terminal.js';
import { InteractiveWindows } from './modules/InteractiveWindows/InteractiveWindows.js';

let windows = document.querySelectorAll('.window');
let interactiveWindows = new InteractiveWindows(windows);

windows.forEach(win => {
    new Terminal(win);
});

document.querySelector('#terminal-icon').addEventListener('click', function() {
    const terminalContainer = document.createElement('div');
    terminalContainer.setAttribute('data-title', 'Terminal');
    terminalContainer.classList.add('window');
    
    const contentDiv = document.querySelector('#content');
    contentDiv.appendChild(terminalContainer);
    
    // Add the new terminal to the existing InteractiveWindow instance
    interactiveWindows.addWindow(terminalContainer);

    new Terminal(terminalContainer);
});