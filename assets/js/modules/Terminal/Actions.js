import { PromptHandler } from './PromptHandler.js';
import { Question } from './Question.js';
import { Conversation } from './Conversation.js';
import { Action } from './Action.js';

export class Actions {
    constructor(terminal) {
        this.help = new Action(() => {
            return 'Supported commands: ' + Object.keys(this).join(', ');
        });

        this.clear = new Action(() => {
            terminal.outputHandler.clearOutput();
            return '';
        });

        this.about = new Action(async () => {
            const originalTypeSpeed = terminal.outputHandler.typeSpeed;
            try {
                const response = await fetch('/assets/templates/text/resume.txt');
                if (!response.ok) {
                    throw new Error("Failed to fetch the resume");
                }
                const result = await response.text();
                terminal.outputHandler.setTypeSpeed(-5);
                terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                    if (!isRunning) {
                        terminal.outputHandler.setTypeSpeed(originalTypeSpeed);
                    }
                });
                return result;
            } catch (error) {
                return `Error: ${error.message}`;
            }
        });

        this.github = new Action(() => {
            const GITHUB_PAGE = 'https://github.com/wooferppk';
            const QUESTION = `
            <a href="${GITHUB_PAGE}" target="_blank">${GITHUB_PAGE}</a>
            <br></br>
            Would you like me to redirect you to that page?
            <br></br>
            Type 'yes' or 'no'`;

            const question = new Question(QUESTION, {
                yes: new Action(() => {
                    window.open(GITHUB_PAGE, '_blank');
                    const promptHandler = new PromptHandler(terminal, `Opening ${GITHUB_PAGE} in a new tab`);
                    promptHandler.handle();
                }),
                no: new Action(() => {
                    const promptHandler = new PromptHandler(terminal, 'Okay, let me know if you need anything else!');
                    promptHandler.handle();
                })
            });
    
            const conversation = new Conversation(question, terminal, terminal.lastCommand());
            conversation.start();
            return '';
        });

        this.linkedin = new Action(() => {
            const LINKEDIN_PAGE = 'https://www.linkedin.com/in/paulmoscuzza/';
            const QUESTION = `
            <a href="${LINKEDIN_PAGE}" target="_blank">${LINKEDIN_PAGE}</a>
            <br></br>
            Would you like me to redirect you to that page?
            <br></br>
            Type 'yes' or 'no'`;

            const question = new Question(QUESTION, {
                yes: new Action(() => {
                    window.open(LINKEDIN_PAGE, '_blank');
                    const promptHandler = new PromptHandler(terminal, `Opening ${LINKEDIN_PAGE} in a new tab`);
                    promptHandler.handle();
                }),
                no: new Action(() => {
                    const promptHandler = new PromptHandler(terminal, 'Okay, let me know if you need anything else!');
                    promptHandler.handle();
                })
            });
    
            const conversation = new Conversation(question, terminal, terminal.lastCommand());
            conversation.start();
            return '';
        });

        this.shutdown = new Action(async () => {
            terminal.outputHandler.clearOutput();
            try {
                const response = await fetch('/assets/templates/text/shutdown.txt');
                if (!response.ok) {
                    throw new Error('Failed to shutdown');
                }

                const result = await response.text();

                terminal.terminalEvents.removeCtrlCListener();

                terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                    if (!isRunning) {
                        setTimeout(() => {
                            terminal.terminalUI.destroyTerminal();
                        }, 2000);
                    }
                });

                return result;
            } catch (error) {
                return `Error: ${error.message}`;
            }
        })
    }
}