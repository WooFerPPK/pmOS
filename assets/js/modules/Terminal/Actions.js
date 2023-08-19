import { PromptHandler } from './PromptHandler.js';
import { Question } from './Question.js';
import { Conversation } from './Conversation.js';
import { Action } from './Action.js';
import StartupManager from '../StartupManager/StartupManager.js';
import { getDayOfWeek, formatDate, formatTime12Hour} from '../../utilities/DateTime.js';
import { initiateDownload } from '../../utilities/Download.js';
import { CalculatingEngine } from '../Calculator/CalculatingEngine.js';
import { GITHUB_PAGE, LINKEDIN_PAGE, RESUME_TXT_PATH, RESUME_PDF_PATH, RESUME_HTML_PATH, RESUME_FILE_NAME, CALCULATOR_PATH } from '../../utilities/Constants.js';
import TemplateLoader from '../TemplateLoader/TemplateLoader.js';

/**
 * Constructs Actions with the provided terminal interface.
 * @param {Object} terminal - The terminal interface for user interaction.
 */
export class Actions {
    /**
     * Constructs Actions with the provided terminal interface.
     * @param {Object} terminal - The terminal interface for user interaction.
     */
    constructor(terminal) {
        /**
         * List and return available terminal commands.
         * @returns {string} Comma-separated list of commands.
         */
        this.help = new Action(() => {
            let commands = Object.keys(this).join(', ');            
            return commands;
        });

        // this.login = new Action(() => {
        //     const desktop = document.querySelector('#desktop');
        //     const loginScreen = new TemplateLoader(desktop, terminal.observable, 'assets/templates/html/LoginScreen/LoginScreen.html', 'loginScreen', (container) => {
        //         container.querySelector('#startPmOS').addEventListener('click', ()=>{
        //             container.remove();
        //         })
        //     });
        //     loginScreen.open();
        //     return 'Testing login'
        // })

        this.about = new Action(() => {
            return `
            Hello! I'm Paul, a seasoned Frontend Developer and Technical Leader based in Toronto. Ever since I was young, I've been drawn to taking things apart and putting them back together. This innate curiosity seamlessly transitioned into a profound passion for web development. Over the past decade, I've honed my expertise in technologies like Node.js, Vue.js, and React, and I'm proud of the high-quality web applications I've brought to life.
            <br></br>
            I've played pivotal roles in crafting state-of-the-art administrative tools and domain management systems, enhancing digital experiences for users everywhere. My portfolio also boasts of projects catering to cinephiles, where I've merged my technical skills with the world of film, providing seamless digital interfaces for film enthusiasts.
            <br></br>
            Beyond the code, I pride myself on being a true team player. My ability to effectively collaborate with diverse cross-functional teams has led to smoother integrations and more efficient projects. When I step away from the screen, my world revolves around video games, the latest tech, cinema, or planning my next travel adventure. With every project and every journey, I carry the thrill of solving a challenge and the joy of seeing people benefit from my efforts.
            <br></br>
            Want to dive deeper?
            <br></br>
            Type 'linkedin' to check out my professional background, 'github' to see some of my code, or 'resume' for a detailed look at my journey.
            <br></br>
            Stuck or curious? Type help for more commands. Happy exploring!
            `
        });

        this.calculate = new Action(() => {
            // Set the free input processor function
            terminal.inputHandler.freeInputProcessor = (input) => {
                const calculateEngine = new CalculatingEngine();
                try {
                    return calculateEngine.evaluate(input);
                } catch (error) {
                    return error;
                }
            };
            return `Enter your mathematical expression. Type 'exit' to leave this mode.`;
        });
        

        this.clear = new Action(() => {
            terminal.outputHandler.clearOutput();
            return '';
        });

        this.resume = new Action(() => {
            const startupManager = new StartupManager(terminal.interactiveWindows, terminal.observable);
            const RESUME_QUESTION = `
                What would you like to do with Paul Moscuzza's resume?
                <br></br>
                1. Download the resume
                <br></br>
                2. View the resume
                <br></br>
                3. Open the resume as a PDF
                <br></br>
                4. View the resume as HTML
                <br></br>
                5. Exit this mode
                <br></br>
                Type the number corresponding to your choice.`;
    
            const question = new Question(RESUME_QUESTION, {
                '1': new Action(() => {
                    initiateDownload(RESUME_PDF_PATH, RESUME_FILE_NAME);
                    const promptHandler = new PromptHandler(terminal, `Initiating download of Paul Moscuzza's resume`);
                    promptHandler.handle();
                }),
                '2': this.createFetchAction(RESUME_TXT_PATH, (result) => {
                    terminal.outputHandler.setTypeSpeed(-5);
                    terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                        if (!isRunning) {
                            terminal.outputHandler.setTypeSpeed(0); // originalTypeSpeed
                            const promptHandler = new PromptHandler(terminal);
                            promptHandler.handle();
                        }
                    });
                    return result;
                }),
                '3': new Action(() => {
                    startupManager.startPDFViewer(RESUME_PDF_PATH, RESUME_HTML_PATH, `PDF Resume`);
                    const promptHandler = new PromptHandler(terminal, `Opening Paul Moscuzza's PDF resume`);
                    promptHandler.handle();
                }),
                '4': new Action(() => {
                    startupManager.startHTMLViewer(RESUME_HTML_PATH, `HTML Resume`);
                    const promptHandler = new PromptHandler(terminal, `Opening HTML version of Paul Moscuzza's resume`);
                    promptHandler.handle();
                }),
                '5': new Action(() => {
                    const promptHandler = new PromptHandler(terminal, 'Exiting resume mode. Let me know if you need anything else!');
                    promptHandler.handle();
                })
            });
    
            const conversation = new Conversation(question, terminal, terminal.lastCommand());
            conversation.start();
            return '';
        });
        
        this['open calculator'] = new Action(() => {
            const startupManager = new StartupManager(terminal.interactiveWindows, terminal.observable);
            startupManager.startHTMLViewer(CALCULATOR_PATH, `Calculator`);
            return `Opening Calculator`;
        });

        this.github = this.createSocialMediaAction(GITHUB_PAGE, terminal);
        this.linkedin = this.createSocialMediaAction(LINKEDIN_PAGE, terminal);

        this.date = new Action(() => {
            const currentDate = new Date();
            return `${getDayOfWeek(currentDate)} ${formatDate(currentDate)} ${formatTime12Hour(currentDate)} `;
        });

        this.exit = new Action(() => {
            terminal.terminalUI.destroyTerminal();
            return '';
        })

        this.shutdown = this.createFetchAction('assets/templates/text/shutdown.txt', (result) => {
            terminal.terminalEvents.removeCtrlCListener();

            terminal.terminalEvents.on('outputRunningChanged', (isRunning) => {
                if (!isRunning) {
                    setTimeout(() => {
                        terminal.observable.notify('windowShutdown', { source: terminal, message: 'shutdown' });
                        terminal.terminalUI.destroyTerminal();
                        const desktop = document.querySelector('#desktop');
                        setTimeout(() => {
                            const loginScreen = new TemplateLoader(desktop, terminal.observable, 'assets/templates/html/LoginScreen/LoginScreen.html', 'loginScreen', (container) => {
                                const loginButton = container.querySelector('#startPmOS');
                                const loginScreenElement = container.querySelector('.loginScreenContent'); 

                                loginButton.addEventListener('click', ()=>{
                                    container.remove();
                                })
                                
                                loginButton.addEventListener('mouseover', () => {
                                    loginScreenElement.style.background = "transparent";
                                });
                                
                                loginButton.addEventListener('mouseleave', () => {
                                    loginScreenElement.style.background = "black";
                                });
                            });
                            loginScreen.open();
                        }, 2000);
                    }, 2000);
                }
            });

            return result;
        });
    }

    createFetchAction(url, onSuccess) {
        return new Action(async () => {
            try {
                const response = await fetch(url);
                if (!response.ok) {
                    throw new Error(`Failed to fetch from ${url}`);
                }
                const result = await response.text();
                return onSuccess(result);
            } catch (error) {
                return `Error: ${error.message}`;
            }
        });
    }

    createSocialMediaAction(pageUrl, terminal) {
        const startupManager = new StartupManager(terminal.interactiveWindows, terminal.observable);
        return new Action(() => {
            const QUESTION = `
                <a href="${pageUrl}" target="_blank">${pageUrl}</a>
                <br></br>
                Would you like me to redirect you to that page?
                <br></br>
                Type 'yes' or 'no'`;

            const questions = new Question(QUESTION, {
                yes: new Action(() => {
                    const promptHandler = new PromptHandler(terminal, `Opening ${pageUrl} in a new tab`);
                    promptHandler.handle();
                    
                    startupManager.startOpenPage(pageUrl);
                }),
                no: new Action(() => {
                    const promptHandler = new PromptHandler(terminal, 'Okay, let me know if you need anything else!');
                    promptHandler.handle();
                })
            });

            const conversation = new Conversation(questions, terminal, terminal.lastCommand());
            conversation.start();
            return '';
        });
    }
}
